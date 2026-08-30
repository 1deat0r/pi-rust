//! Default stream-function registry — port of
//! `packages/agent/src/stream-fn.ts`.
//!
//! Hosts that provide a default model runtime can install its stream function
//! here so low-level loops can omit an explicit one without pi-agent-core
//! depending on a provider catalog.

use std::sync::RwLock;

use crate::agent::StreamFn;

static DEFAULT_STREAM_FN: std::sync::LazyLock<RwLock<Option<StreamFn>>> =
    std::sync::LazyLock::new(|| RwLock::new(None));

/// Install the fallback stream function used when callers omit `streamFn`.
pub fn set_default_stream_fn(stream_fn: StreamFn) {
    *DEFAULT_STREAM_FN
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(stream_fn);
}

/// Clear the default stream function (mostly useful in tests).
pub fn clear_default_stream_fn() {
    *DEFAULT_STREAM_FN
        .write()
        .unwrap_or_else(|error| error.into_inner()) = None;
}

/// Return the configured default stream function or panic with the upstream
/// message (mirrors `getDefaultStreamFn`).
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
pub fn get_default_stream_fn() -> StreamFn {
    let slot = DEFAULT_STREAM_FN
        .read()
        .unwrap_or_else(|error| error.into_inner());
    match slot.as_ref() {
        Some(s) => s.clone(),
        None => panic!(
            "No default stream function configured. Pass streamFn explicitly or call setDefaultStreamFn()."
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_ai::types::Context;

    fn dummy_stream() -> StreamFn {
        std::sync::Arc::new(|_model: &pi_ai::model::Model, _ctx: &Context| {
            pi_ai::event_stream::create_error_stream(
                "faux",
                "faux",
                "faux-1",
                "not used".to_string(),
            )
        })
    }

    #[test]
    #[should_panic(expected = "No default stream function configured")]
    fn unset_default_stream_fn_panics() {
        clear_default_stream_fn();
        let _ = get_default_stream_fn();
    }

    #[test]
    fn set_and_get_default_stream_fn() {
        let s = dummy_stream();
        set_default_stream_fn(s.clone());
        let g = get_default_stream_fn();
        assert!(std::sync::Arc::ptr_eq(&s, &g));
        clear_default_stream_fn();
    }
}
