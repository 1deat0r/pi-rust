//! Per-run environment visible to subprocess tools.
//!
//! Pi exposes the active session and model to commands launched by built-in
//! tools.  The values must be scoped to one coding-agent invocation: mutating
//! the process environment without restoring the previous values would leak
//! stale session metadata into later in-process tests or embedded callers.

use std::collections::BTreeMap;
use std::sync::Mutex;

const SESSION_KEYS: [&str; 5] = [
    "PI_SESSION_ID",
    "PI_SESSION_FILE",
    "PI_PROVIDER",
    "PI_MODEL",
    "PI_REASONING_LEVEL",
];

#[derive(Debug)]
pub struct SessionEnvironmentGuard {
    previous: BTreeMap<String, Option<String>>,
    /// Values most recently written by this guard.  Keeping these separate
    /// from `previous` lets Drop avoid restoring over a value that another
    /// owner installed after this guard was created (important for embedded
    /// callers that host more than one runtime in one process).
    active: Mutex<BTreeMap<String, String>>,
}

/// Install the active session/model values for child processes and return a
/// guard that restores every prior value on drop.
pub fn install(
    session_id: &str,
    session_file: &str,
    provider: &str,
    model: &str,
    reasoning_level: &str,
) -> SessionEnvironmentGuard {
    let values = [
        ("PI_SESSION_ID", session_id),
        ("PI_SESSION_FILE", session_file),
        ("PI_PROVIDER", provider),
        ("PI_MODEL", model),
        ("PI_REASONING_LEVEL", reasoning_level),
    ];
    let mut previous = BTreeMap::new();
    let mut active = BTreeMap::new();
    for (key, value) in values {
        previous.insert(key.to_string(), std::env::var(key).ok());
        active.insert(key.to_string(), value.to_string());
        // SAFETY: the guard scopes these process-wide values to the current
        // run and restores them before returning to the embedding process.
        unsafe { std::env::set_var(key, value) };
    }
    SessionEnvironmentGuard {
        previous,
        active: Mutex::new(active),
    }
}

impl Drop for SessionEnvironmentGuard {
    fn drop(&mut self) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in SESSION_KEYS {
            // Do not clobber a value written by a different owner after this
            // guard installed its snapshot.  Separate OS processes have
            // naturally isolated environments; this check covers embedded
            // Rust callers and nested guards without changing the public API.
            if std::env::var(key).ok().as_deref() != active.get(key).map(String::as_str) {
                continue;
            }
            match self.previous.get(key).and_then(Clone::clone) {
                Some(value) => {
                    // SAFETY: restoration mirrors the scoped installation.
                    unsafe { std::env::set_var(key, value) };
                }
                None => {
                    // SAFETY: restoration mirrors the scoped installation.
                    unsafe { std::env::remove_var(key) };
                }
            }
        }
    }
}

impl SessionEnvironmentGuard {
    /// Update a runtime field while retaining the original value for Drop.
    pub fn set_reasoning_level(&self, level: &str) {
        // SAFETY: the guard remains responsible for restoring the original
        // process value when the owning mode exits.
        unsafe { std::env::set_var("PI_REASONING_LEVEL", level) };
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert("PI_REASONING_LEVEL".to_string(), level.to_string());
    }

    pub fn set_model(&self, provider: &str, model: &str) {
        // SAFETY: see `set_reasoning_level`.
        unsafe {
            std::env::set_var("PI_PROVIDER", provider);
            std::env::set_var("PI_MODEL", model);
        }
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.insert("PI_PROVIDER".to_string(), provider.to_string());
        active.insert("PI_MODEL".to_string(), model.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_scopes_and_restores_all_values() {
        let previous = SESSION_KEYS
            .into_iter()
            .map(|key| (key, std::env::var(key).ok()))
            .collect::<BTreeMap<_, _>>();
        let guard = install("session", "/tmp/session.jsonl", "faux", "faux-1", "high");
        assert_eq!(std::env::var("PI_SESSION_ID").as_deref(), Ok("session"));
        assert_eq!(
            std::env::var("PI_SESSION_FILE").as_deref(),
            Ok("/tmp/session.jsonl")
        );
        assert_eq!(std::env::var("PI_PROVIDER").as_deref(), Ok("faux"));
        assert_eq!(std::env::var("PI_MODEL").as_deref(), Ok("faux-1"));
        assert_eq!(std::env::var("PI_REASONING_LEVEL").as_deref(), Ok("high"));
        drop(guard);
        for (key, value) in previous {
            assert_eq!(std::env::var(key).ok(), value);
        }
    }

    #[test]
    fn real_child_process_receives_scoped_session_values() {
        let guard = install("child-session", "/tmp/child.jsonl", "faux", "faux-1", "low");
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", "printf '%s|%s|%s|%s|%s' \"$PI_SESSION_ID\" \"$PI_SESSION_FILE\" \"$PI_PROVIDER\" \"$PI_MODEL\" \"$PI_REASONING_LEVEL\""])
            .output()
            .expect("child shell should start");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "child-session|/tmp/child.jsonl|faux|faux-1|low"
        );
        drop(guard);
    }

    #[test]
    fn drop_does_not_restore_over_a_newer_owner_value() {
        let previous = SESSION_KEYS
            .into_iter()
            .map(|key| (key, std::env::var(key).ok()))
            .collect::<BTreeMap<_, _>>();
        let guard = install(
            "owned-session",
            "/tmp/owned.jsonl",
            "faux",
            "faux-1",
            "medium",
        );
        // SAFETY: this test restores every touched variable below.  The
        // mutation represents a second embedded runtime taking ownership.
        unsafe { std::env::set_var("PI_MODEL", "newer-owner-model") };
        drop(guard);
        assert_eq!(
            std::env::var("PI_MODEL").as_deref(),
            Ok("newer-owner-model")
        );
        for (key, value) in previous {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}
