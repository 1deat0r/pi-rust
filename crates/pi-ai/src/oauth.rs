//! OAuth flow machinery — port of `packages/ai/src/auth/oauth/`
//! (`device-code.ts` + `pkce.ts`).
//!
//! Supplies the RFC 8628 device-code polling loop (with slow_down
//! semantics and abortable sleeps) and PKCE verifier/challenge generation.
//! Provider-specific endpoint definitions (anthropic, github-copilot,
//! openai-codex, radius, ...) layer on top of these primitives.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEVICE_CODE_CANCEL_MESSAGE: &str = "Login cancelled";
pub const DEVICE_CODE_TIMEOUT_MESSAGE: &str = "Device flow timed out";
pub const DEVICE_CODE_SLOW_DOWN_TIMEOUT_MESSAGE: &str =
    "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
/// RFC 8628 section 3.2: if the authorization server omits `interval`, the
/// client must use 5 seconds.
pub const DEVICE_CODE_MINIMUM_INTERVAL_MS: u64 = 1000;
pub const DEVICE_CODE_DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 section 3.5: `slow_down` increases the polling interval by 5s.
pub const DEVICE_CODE_SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// One incomplete poll result (RFC 8628 `pending` / `slow_down` / failure)
/// or a completed value.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceCodePollResult<T> {
    Pending,
    SlowDown { interval_seconds: Option<f64> },
    Failed { message: String },
    Complete(T),
}

/// Poll closure: `FnMut()` returning a future that yields the poll result.
pub type DeviceCodePollFn<T> =
    Box<dyn FnMut() -> Pin<Box<dyn Future<Output = DeviceCodePollResult<T>> + Send>> + Send>;

/// Options for `poll_oauth_device_code_flow` (upstream
/// `OAuthDeviceCodePollOptions`).
pub struct DeviceCodePollOptions<'a, T> {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<u64>,
    pub wait_before_first_poll: bool,
    pub poll: DeviceCodePollFn<T>,
    /// Abort signal (port of `AbortSignal` as a sync flag).
    pub signal: Option<Arc<AtomicBool>>,
    __marker: std::marker::PhantomData<&'a T>,
}

impl<'a, T> DeviceCodePollOptions<'a, T> {
    pub fn new(poll: DeviceCodePollFn<T>) -> Self {
        Self {
            interval_seconds: None,
            expires_in_seconds: None,
            wait_before_first_poll: false,
            poll,
            signal: None,
            __marker: std::marker::PhantomData,
        }
    }
}

/// Sleep that aborts early when the signal fires (upstream `abortableSleep`).
pub async fn abortable_sleep(
    ms: u64,
    signal: Option<&Arc<AtomicBool>>,
    cancel_message: &str,
) -> Result<(), String> {
    if signal.map(|s| s.load(Ordering::SeqCst)).unwrap_or(false) {
        return Err(cancel_message.to_string());
    }
    let sleep = tokio::time::sleep(Duration::from_millis(ms));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if signal.map(|s| s.load(Ordering::SeqCst)).unwrap_or(false) {
                    return Err(cancel_message.to_string());
                }
            }
        }
    }
}

/// Poll an OAuth device-code flow until completion, failure, or timeout
/// (upstream `pollOAuthDeviceCodeFlow`, RFC 8628).
pub async fn poll_oauth_device_code_flow<T>(
    options: &mut DeviceCodePollOptions<'_, T>,
) -> Result<T, String> {
    let deadline = match options.expires_in_seconds {
        Some(seconds) => Instant::now() + Duration::from_secs(seconds),
        None => Instant::now() + Duration::from_secs(u64::MAX / 2),
    };
    let mut interval_ms = (options
        .interval_seconds
        .unwrap_or(DEVICE_CODE_DEFAULT_POLL_INTERVAL_SECONDS)
        * 1000.0)
        .floor()
        .max(DEVICE_CODE_MINIMUM_INTERVAL_MS as f64) as u64;
    let mut slow_down_responses = 0u32;

    if options.wait_before_first_poll {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            abortable_sleep(
                interval_ms.min(remaining.as_millis() as u64),
                options.signal.as_ref(),
                DEVICE_CODE_CANCEL_MESSAGE,
            )
            .await?;
        }
    }

    loop {
        if options
            .signal
            .as_ref()
            .map(|s| s.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            return Err(DEVICE_CODE_CANCEL_MESSAGE.to_string());
        }
        let result = (options.poll)().await;
        match result {
            DeviceCodePollResult::Complete(value) => return Ok(value),
            DeviceCodePollResult::Failed { message } => return Err(message),
            DeviceCodePollResult::Pending => {}
            DeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                // Server-provided interval wins (GitHub reports the new
                // required minimum); otherwise increment by 5s (RFC 8628 §3.5).
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds.is_finite() && seconds > 0.0 => (seconds * 1000.0)
                        .floor()
                        .max(DEVICE_CODE_MINIMUM_INTERVAL_MS as f64)
                        as u64,
                    _ => interval_ms + DEVICE_CODE_SLOW_DOWN_INTERVAL_INCREMENT_MS,
                };
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        abortable_sleep(
            interval_ms.min(remaining.as_millis() as u64),
            options.signal.as_ref(),
            DEVICE_CODE_CANCEL_MESSAGE,
        )
        .await?;
    }

    if slow_down_responses > 0 {
        Err(DEVICE_CODE_SLOW_DOWN_TIMEOUT_MESSAGE.to_string())
    } else {
        Err(DEVICE_CODE_TIMEOUT_MESSAGE.to_string())
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// Base64url (no padding) encode — upstream `base64urlEncode`.
pub fn base64url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a PKCE verifier + S256 challenge (upstream `generatePKCE`).
pub fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    let rng = ring::rand::SystemRandom::new();
    ring::rand::SecureRandom::fill(&rng, &mut verifier_bytes)
        .expect("system random should fill the verifier");
    let verifier = base64url_encode(&verifier_bytes);
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest as _;
    hasher.update(verifier.as_bytes());
    let challenge = base64url_encode(&hasher.finalize());
    (verifier, challenge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_shapes() {
        let (verifier, challenge) = generate_pkce();
        assert_eq!(verifier.len(), 43, "verifier {verifier}");
        assert_eq!(challenge.len(), 43, "challenge {challenge}");
        assert_ne!(verifier, challenge);
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[tokio::test]
    async fn completes_on_first_poll() {
        let mut options = DeviceCodePollOptions::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::Complete(42u32) })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(30);
        let value = poll_oauth_device_code_flow(&mut options).await.unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn handles_pending_slow_down_then_complete() {
        let states: Vec<DeviceCodePollResult<&'static str>> = vec![
            DeviceCodePollResult::Pending,
            DeviceCodePollResult::SlowDown {
                interval_seconds: None,
            },
            DeviceCodePollResult::SlowDown {
                interval_seconds: Some(1.0),
            },
            DeviceCodePollResult::Complete("done"),
        ];
        let mut index = 0usize;
        let mut options = DeviceCodePollOptions::new(Box::new(move || {
            let state = states
                .get(index)
                .cloned()
                .unwrap_or(DeviceCodePollResult::Complete("done"));
            index += 1;
            Box::pin(async move { state })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(30);
        let start = Instant::now();
        let value = poll_oauth_device_code_flow(&mut options).await.unwrap();
        assert_eq!(value, "done");
        // Pending -> 1s sleep; SlowDown(None) -> interval += 5s but the NEXT
        // sleep is min(6s, remaining) ... server SlowDown(1.0) resets to 1s,
        // then complete. Expect at least ~2s of sleeps.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1900),
            "elapsed {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(12), "elapsed {elapsed:?}");
    }

    #[tokio::test]
    async fn aborts_with_signal() {
        let signal = Arc::new(AtomicBool::new(false));
        let sig = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            sig.store(true, Ordering::SeqCst);
        });
        let mut options = DeviceCodePollOptions::<()>::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::<()>::Pending })
        }));
        options.interval_seconds = Some(30.0);
        options.expires_in_seconds = Some(60);
        options.signal = Some(signal);
        let err = poll_oauth_device_code_flow(&mut options).await.unwrap_err();
        assert_eq!(err, DEVICE_CODE_CANCEL_MESSAGE);
    }

    #[tokio::test]
    async fn timeout_when_all_pending() {
        let mut options = DeviceCodePollOptions::<()>::new(Box::new(move || {
            Box::pin(async { DeviceCodePollResult::<()>::Pending })
        }));
        options.interval_seconds = Some(1.0);
        options.expires_in_seconds = Some(2);
        let err = poll_oauth_device_code_flow(&mut options).await.unwrap_err();
        assert_eq!(err, DEVICE_CODE_TIMEOUT_MESSAGE);
    }

    #[test]
    fn base64url_no_padding() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"f\xfb"), "Zvs");
        assert_eq!(base64url_encode(b"f\xfb\xff"), "Zvv_");
    }
}
