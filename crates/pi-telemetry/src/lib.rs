//! Vendor-neutral telemetry contracts and reference adapters.
//!
//! Port of `@earendil-works/pi-telemetry`: `index.ts` (contract types),
//! `noop.ts`, and `memory.ts` (in-memory reference implementation).
//!
//! Rust interface note: upstream's `TelemetrySpan`/`TelemetryContext` are
//! callback-passing interfaces. Here the recording trait (`TelemetrySpan`)
//! is object-safe (add_event/set_attributes/set_status), while contexts
//! implement `TelemetryContext::start_span` as a generic method — the TS
//! object-identity rules (no-op when parent settled, passive recording,
//! automatic error status, settle-once sequencing) are preserved.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub type AttributeValue = serde_json::Value;
pub type SpanAttributes = BTreeMap<String, AttributeValue>;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: Option<SpanAttributes>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SpanStatus {
    Ok,
    Error { error: Option<SpanError> },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpanError {
    pub name: String,
    pub message: String,
}

/// Recording surface of a span. Object-safe; callbacks are layered on top by
/// concrete context implementations.
pub trait TelemetrySpan: Send + Sync {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
}

/// Start a span scoped to a callback. The callback receives the live span
/// handle (`&dyn TelemetrySpan`) and returns the callback's own result.
pub trait TelemetryContext: Send + Sync {
    fn start_span<T, F>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&dyn TelemetrySpan) -> T;
}

// ---------------------------------------------------------------------------
// Noop
// ---------------------------------------------------------------------------

struct NoopTelemetrySpan;

impl TelemetrySpan for NoopTelemetrySpan {
    fn add_event(&self, _name: &str, _attributes: Option<SpanAttributes>) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}
}

/// Shared telemetry context used when an application does not provide one.
pub struct NoopTelemetry;

impl TelemetryContext for NoopTelemetry {
    fn start_span<T, F>(&self, _options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&dyn TelemetrySpan) -> T,
    {
        let span: &dyn TelemetrySpan = &NoopTelemetrySpan;
        callback(span)
    }
}

/// The upstream `NOOP_TELEMETRY_CONTEXT` equivalent.
pub const NOOP_TELEMETRY_CONTEXT: NoopTelemetry = NoopTelemetry;

// ---------------------------------------------------------------------------
// In-memory reference implementation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    pub end_sequence: Option<u64>,
}

#[derive(Debug)]
struct MutableRecordedTelemetrySpan {
    id: u64,
    parent_id: Option<u64>,
    name: String,
    attributes: SpanAttributes,
    events: Vec<RecordedTelemetryEvent>,
    status: SpanStatus,
    explicit_status: bool,
    settled: bool,
    end_sequence: Option<u64>,
}

#[derive(Debug, Default)]
struct InMemoryTelemetryState {
    spans: Vec<MutableRecordedTelemetrySpan>,
    next_span_id: u64,
    next_end_sequence: u64,
}

impl Default for MutableRecordedTelemetrySpan {
    fn default() -> Self {
        Self {
            id: 0,
            parent_id: None,
            name: String::new(),
            attributes: SpanAttributes::new(),
            events: Vec::new(),
            status: SpanStatus::Ok,
            explicit_status: false,
            settled: false,
            end_sequence: None,
        }
    }
}

fn copy_attributes(attributes: Option<&SpanAttributes>) -> SpanAttributes {
    attributes.cloned().unwrap_or_default()
}

fn merge_attributes(current: &SpanAttributes, attributes: &SpanAttributes) -> SpanAttributes {
    let mut merged = current.clone();
    for (name, value) in attributes {
        merged.insert(name.clone(), value.clone());
    }
    merged
}

fn automatic_error_status(error: Option<&(dyn Error + Send + Sync)>) -> SpanStatus {
    if let Some(err) = error {
        SpanStatus::Error {
            error: Some(SpanError {
                name: "Error".to_string(),
                message: err.to_string(),
            }),
        }
    } else {
        SpanStatus::Error { error: None }
    }
}

fn settle_span(
    state: &mut InMemoryTelemetryState,
    index: usize,
    failed: bool,
    error: Option<&(dyn Error + Send + Sync)>,
) {
    let span = &mut state.spans[index];
    if span.settled {
        return;
    }
    if failed && !span.explicit_status {
        span.status = automatic_error_status(error);
    }
    span.settled = true;
    span.end_sequence = Some(state.next_end_sequence);
    state.next_end_sequence += 1;
}

/// Handle passed to callbacks. Records through the shared state and honors
/// the upstream "passive recording, no recording after settle" rules.
struct InMemorySpanHandle {
    state: Arc<Mutex<InMemoryTelemetryState>>,
    index: usize,
    settled: Arc<AtomicBool>,
}

impl TelemetrySpan for InMemorySpanHandle {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>) {
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = self.state.lock().unwrap();
        let rec = &mut guard.spans[self.index];
        if rec.settled {
            return;
        }
        rec.events.push(RecordedTelemetryEvent {
            name: name.to_string(),
            attributes: copy_attributes(attributes.as_ref()),
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = self.state.lock().unwrap();
        let rec = &mut guard.spans[self.index];
        if rec.settled {
            return;
        }
        rec.attributes = merge_attributes(&rec.attributes, &attributes);
    }

    fn set_status(&self, status: SpanStatus) {
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = self.state.lock().unwrap();
        let rec = &mut guard.spans[self.index];
        if rec.settled {
            return;
        }
        rec.status = status;
        rec.explicit_status = true;
    }
}

/// Live child span handle with nested span support (mirrors `TelemetrySpan`
/// also being a `TelemetryContext` upstream).
#[allow(dead_code)] // state/index/settled reserved for span-correlation bookkeeping
struct InMemoryChildSpan {
    handle: Arc<dyn TelemetrySpan + Send + Sync>,
    state: Arc<Mutex<InMemoryTelemetryState>>,
    index: usize,
    settled: Arc<AtomicBool>,
}

impl TelemetrySpan for InMemoryChildSpan {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>) {
        self.handle.add_event(name, attributes);
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        self.handle.set_attributes(attributes);
    }
    fn set_status(&self, status: SpanStatus) {
        self.handle.set_status(status);
    }
}

/// Backend-neutral reference implementation that records spans in process
/// memory. Create a fresh instance to isolate tests or recording scopes.
#[derive(Debug, Clone)]
pub struct InMemoryTelemetryContext {
    state: Arc<Mutex<InMemoryTelemetryState>>,
}

impl Default for InMemoryTelemetryContext {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTelemetryContext {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryTelemetryState::default())),
        }
    }

    fn start_span_inner<T, F>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&dyn TelemetrySpan) -> T,
    {
        // Create the record.
        let mut guard = self.state.lock().unwrap();
        let id = guard.next_span_id;
        guard.next_span_id += 1;
        let index = guard.spans.len();
        guard.spans.push(MutableRecordedTelemetrySpan {
            id,
            name: options.name.clone(),
            attributes: copy_attributes(options.attributes.as_ref()),
            ..Default::default()
        });
        drop(guard);

        let settled = Arc::new(AtomicBool::new(false));
        let handle = InMemorySpanHandle {
            state: self.state.clone(),
            index,
            settled: settled.clone(),
        };
        let handle: Arc<dyn TelemetrySpan + Send + Sync> = Arc::new(handle);
        let child_span = InMemoryChildSpan {
            handle: handle.clone(),
            state: self.state.clone(),
            index,
            settled: settled.clone(),
        };

        let result = {
            let span: &dyn TelemetrySpan = &child_span;
            callback(span)
        };
        settled.store(true, Ordering::SeqCst);
        let mut guard = self.state.lock().unwrap();
        settle_span(&mut guard, index, false, None);
        result
    }

    /// Returns detached snapshots in span-start order.
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        let guard = self.state.lock().unwrap();
        guard
            .spans
            .iter()
            .map(|s| RecordedTelemetrySpan {
                id: s.id,
                parent_id: s.parent_id,
                name: s.name.clone(),
                attributes: s.attributes.clone(),
                events: s
                    .events
                    .iter()
                    .map(|e| RecordedTelemetryEvent {
                        name: e.name.clone(),
                        attributes: e.attributes.clone(),
                    })
                    .collect(),
                status: s.status.clone(),
                settled: s.settled,
                end_sequence: s.end_sequence,
            })
            .collect()
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn start_span<T, F>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&dyn TelemetrySpan) -> T,
    {
        self.start_span_inner(options, callback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_runs_callback() {
        let mut called = false;
        NOOP_TELEMETRY_CONTEXT.start_span(
            SpanOptions { name: "n".into(), attributes: None },
            |span| {
                span.add_event("ev", None);
                called = true;
                42
            },
        );
        assert!(called);
    }

    #[test]
    fn memory_records_spans() {
        let ctx = InMemoryTelemetryContext::new();
        ctx.start_span(
            SpanOptions { name: "root".into(), attributes: None },
            |root| {
                root.add_event("start", None);
            },
        );
        let spans = ctx.get_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "root");
        assert_eq!(spans[0].parent_id, None);
        assert!(spans[0].settled);
        assert_eq!(spans[0].events.len(), 1);
        assert_eq!(spans[0].events[0].name, "start");
    }

    #[test]
    fn memory_records_attributes_and_status() {
        let ctx = InMemoryTelemetryContext::new();
        ctx.start_span(
            SpanOptions {
                name: "s".into(),
                attributes: Some(BTreeMap::from([("a".into(), serde_json::json!(1))])),
            },
            |span| {
                span.set_attributes(BTreeMap::from([("b".into(), serde_json::json!("x"))]));
                span.set_status(SpanStatus::Error { error: None });
            },
        );
        let span = &ctx.get_spans()[0];
        assert_eq!(span.attributes.get("a"), Some(&serde_json::json!(1)));
        assert_eq!(span.attributes.get("b"), Some(&serde_json::json!("x")));
        assert_eq!(span.status, SpanStatus::Error { error: None });
    }
}
