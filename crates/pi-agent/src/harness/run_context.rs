//! Run context — port of `packages/agent/src/harness/context.ts` (0.85.1).
//!
//! Upstream threads a `Context` (cancellation + scoped values + telemetry)
//! through filesystem, tools, and stream options instead of bare
//! `AbortSignal`s. The Rust port models the same three capabilities without
//! the JS `AsyncLocalStorage` machinery: explicit parent/child cancel
//! propagation via a shared flag, a string-keyed JSON value map, and an
//! optional deadline timestamp.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Cancellation + value scope for one harness operation (upstream `Context`).
#[derive(Debug, Clone)]
pub struct RunContext {
    cancelled: Arc<AtomicBool>,
    values: Arc<Mutex<BTreeMap<String, serde_json::Value>>>,
    deadline_ms: Option<u64>,
}

impl Default for RunContext {
    fn default() -> Self {
        Self::background()
    }
}

impl RunContext {
    /// Root context with no deadline and no values (upstream
    /// `BACKGROUND_CONTEXT`).
    pub fn background() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            values: Arc::new(Mutex::new(BTreeMap::new())),
            deadline_ms: None,
        }
    }

    /// Derive a child sharing the parent's cancellation flag. Cancelling the
    /// parent cancels the child (one direction, matching upstream
    /// `withCancel` derivation).
    pub fn child(&self) -> Self {
        Self {
            cancelled: self.cancelled.clone(),
            values: Arc::new(Mutex::new(
                self.values
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone(),
            )),
            deadline_ms: self.deadline_ms,
        }
    }

    /// Attach a scoped value, returning the derived context (upstream
    /// `withContextValue`).
    pub fn with_value(&self, key: &str, value: serde_json::Value) -> Self {
        let mut values = self
            .values
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        values.insert(key.to_string(), value);
        Self {
            cancelled: self.cancelled.clone(),
            values: Arc::new(Mutex::new(values)),
            deadline_ms: self.deadline_ms,
        }
    }

    /// Read a scoped value (upstream `context.value(key)`).
    pub fn value(&self, key: &str) -> Option<serde_json::Value> {
        self.values
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .cloned()
    }

    /// Signal cancellation (upstream abort of the context's signal).
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was signalled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Optional absolute deadline in epoch milliseconds.
    pub fn deadline_ms(&self) -> Option<u64> {
        self.deadline_ms
    }

    /// Derive a context with an absolute deadline (upstream timeout wiring).
    pub fn with_deadline_ms(&self, deadline_ms: u64) -> Self {
        Self {
            cancelled: self.cancelled.clone(),
            values: self.values.clone(),
            deadline_ms: Some(deadline_ms),
        }
    }
}

/// Stable harness identity for one logical tool call, unchanged during safe
/// replay (upstream `AgentHarnessToolInvocation`). Memos are
/// invocation-scoped durable replay notes keyed by name.
#[derive(Debug, Clone)]
pub struct AgentHarnessToolInvocation {
    /// Opaque session-unique id equal to the call's reserved result-entry id.
    pub invocation_id: String,
    pub operation_id: String,
    pub turn_id: String,
    memos: Arc<Mutex<BTreeMap<String, serde_json::Value>>>,
}

impl AgentHarnessToolInvocation {
    pub fn new(
        invocation_id: impl Into<String>,
        operation_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            invocation_id: invocation_id.into(),
            operation_id: operation_id.into(),
            turn_id: turn_id.into(),
            memos: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Read one invocation-scoped durable replay memo (upstream `getMemo`).
    pub async fn get_memo(&self, name: &str) -> Option<serde_json::Value> {
        self.memos
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
    }

    /// Set or delete one invocation-scoped durable replay memo (upstream
    /// `setMemo`; `None` deletes).
    pub async fn set_memo(&self, name: &str, value: Option<serde_json::Value>) {
        let mut memos = self.memos.lock().unwrap_or_else(|error| error.into_inner());
        match value {
            Some(value) => {
                memos.insert(name.to_string(), value);
            }
            None => {
                memos.remove(name);
            }
        }
    }
}
