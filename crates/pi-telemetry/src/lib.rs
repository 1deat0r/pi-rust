//! Vendor-neutral telemetry contracts and reference adapters.
//!
//! Port of `@earendil-works/pi-telemetry`: `index.ts` (contract types),
//! `noop.ts`, and `memory.ts` (in-memory reference implementation).
//!
//! Rust interface note: upstream's `TelemetrySpan`/`TelemetryContext` are
//! callback-passing interfaces where a span is itself a context (you can
//! start child spans on a span). This port models the same identity with a
//! concrete `SpanHandle` passed to callbacks: it implements both the
//! recording trait (`TelemetrySpan`) and `TelemetryContext` (child spans).
//! The recording trait stays object-safe for storage; context start is a
//! concrete method so nested spans work exactly like upstream (parent id
//! correlation, settle-once semantics, no-op after settle).

use std::collections::BTreeMap;
use std::error::Error;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;

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

/// Recording surface of a span. Object-safe; concrete contexts layer
/// child-span creation on top via `SpanHandle`.
pub trait TelemetrySpan: Send + Sync {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
}

/// Start a span scoped to a callback. The callback receives the live span
/// handle (`&SpanHandle`), which is itself a context for child spans, and
/// returns the callback's own result.
pub trait TelemetryContext: Send + Sync {
    fn start_span<T, F>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&SpanHandle) -> T;
}

// ---------------------------------------------------------------------------
// SpanHandle — the concrete span identity passed to callbacks
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum SpanInner {
    Noop,
    InMemory(InMemoryChildSpan),
}

/// Concrete span handle. Behaves as both a recording span and a context.
#[derive(Clone)]
pub struct SpanHandle {
    inner: SpanInner,
}

impl TelemetrySpan for SpanHandle {
    fn add_event(&self, name: &str, attributes: Option<SpanAttributes>) {
        match &self.inner {
            SpanInner::Noop => {}
            SpanInner::InMemory(child) => child.record_add_event(name, attributes),
        }
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        match &self.inner {
            SpanInner::Noop => {}
            SpanInner::InMemory(child) => child.record_set_attributes(attributes),
        }
    }
    fn set_status(&self, status: SpanStatus) {
        match &self.inner {
            SpanInner::Noop => {}
            SpanInner::InMemory(child) => child.record_set_status(status),
        }
    }
}

impl TelemetryContext for SpanHandle {
    fn start_span<T, F>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&SpanHandle) -> T,
    {
        match &self.inner {
            SpanInner::Noop => callback(self),
            SpanInner::InMemory(child) => child.start_chapter(options, callback),
        }
    }
}

impl SpanHandle {
    pub fn noop() -> Self {
        Self {
            inner: SpanInner::Noop,
        }
    }

    /// Start a child span whose lifetime follows an async callback.
    ///
    /// The synchronous `TelemetryContext::start_span` API remains the
    /// canonical contract, but Rust callers must keep a span open across an
    /// `.await` without settling it when the callback merely returns a
    /// future. This helper provides that lifetime bridge.
    pub async fn start_span_async<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(SpanHandle) -> Fut,
        Fut: Future<Output = T>,
    {
        match &self.inner {
            SpanInner::Noop => callback(SpanHandle::noop()).await,
            SpanInner::InMemory(child) => child.start_chapter_async(options, callback).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Noop
// ---------------------------------------------------------------------------

/// Shared telemetry context used when an application does not provide one.
pub struct NoopTelemetry;

impl NoopTelemetry {
    /// Async counterpart to [`TelemetryContext::start_span`].
    pub async fn start_span_async<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(SpanHandle) -> Fut,
        Fut: Future<Output = T>,
    {
        let _ = options;
        callback(SpanHandle::noop()).await
    }
}

impl TelemetryContext for NoopTelemetry {
    fn start_span<T, F>(&self, _options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&SpanHandle) -> T,
    {
        let handle = SpanHandle::noop();
        callback(&handle)
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
    let next = state.next_end_sequence + 1;
    state.next_end_sequence = next;
}

fn settle_panicked_span(state: &mut InMemoryTelemetryState, index: usize) {
    let span = &mut state.spans[index];
    if span.settled {
        return;
    }
    if !span.explicit_status {
        // A panic is the Rust equivalent of an exception that escapes the
        // callback. Keep the payload opaque: telemetry settlement must not
        // inspect or retain arbitrary panic values.
        span.status = SpanStatus::Error { error: None };
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
        Self::record_event(self, name, attributes)
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        Self::record_attributes(self, attributes)
    }
    fn set_status(&self, status: SpanStatus) {
        Self::record_status(self, status)
    }
}

impl InMemorySpanHandle {
    fn record_event(&self, name: &str, attributes: Option<SpanAttributes>) {
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
    fn record_attributes(&self, attributes: SpanAttributes) {
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
    fn record_status(&self, status: SpanStatus) {
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

/// Child-span chapter: records against the shared state and can start its own
/// children (span-as-context, upstream `TelemetrySpan.startSpan`).
struct InMemoryChildSpan {
    record: Arc<dyn TelemetrySpan + Send + Sync>,
    state: Arc<Mutex<InMemoryTelemetryState>>,
    index: usize,
}

impl Clone for InMemoryChildSpan {
    fn clone(&self) -> Self {
        Self {
            record: self.record.clone(),
            state: self.state.clone(),
            index: self.index,
        }
    }
}

impl InMemoryChildSpan {
    fn record_add_event(&self, name: &str, attributes: Option<SpanAttributes>) {
        self.record.add_event(name, attributes);
    }
    fn record_set_attributes(&self, attributes: SpanAttributes) {
        self.record.set_attributes(attributes);
    }
    fn record_set_status(&self, status: SpanStatus) {
        self.record.set_status(status);
    }

    /// Start a child span underneath this one. Upstream: a settled parent
    /// silently degrades to the NOOP context.
    fn start_chapter<T, F>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(&SpanHandle) -> T,
    {
        let parent_id = {
            let guard = self.state.lock().unwrap();
            if guard.spans[self.index].settled {
                let handle = SpanHandle::noop();
                return callback(&handle);
            }
            Some(guard.spans[self.index].id)
        };
        start_span_with_parent(&self.state, parent_id, options, callback)
    }

    async fn start_chapter_async<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(SpanHandle) -> Fut,
        Fut: Future<Output = T>,
    {
        let parent_id = {
            let guard = self.state.lock().unwrap();
            if guard.spans[self.index].settled {
                return callback(SpanHandle::noop()).await;
            }
            Some(guard.spans[self.index].id)
        };
        start_span_with_parent_async(&self.state, parent_id, options, callback).await
    }
}

/// Shared record-creation + callback machinery for root, child, and deeper
/// spans. Mirrors upstream `startInMemorySpan` including settle-once.
fn start_span_with_parent<T, F>(
    state: &Arc<Mutex<InMemoryTelemetryState>>,
    parent_id: Option<u64>,
    options: SpanOptions,
    callback: F,
) -> T
where
    F: FnOnce(&SpanHandle) -> T,
{
    // Create the record.
    let mut guard = state.lock().unwrap();
    let id = guard.next_span_id;
    guard.next_span_id += 1;
    let index = guard.spans.len();
    guard.spans.push(MutableRecordedTelemetrySpan {
        id,
        parent_id,
        name: options.name.clone(),
        attributes: copy_attributes(options.attributes.as_ref()),
        ..Default::default()
    });
    drop(guard);

    let settled = Arc::new(AtomicBool::new(false));
    let handle = InMemorySpanHandle {
        state: state.clone(),
        index,
        settled: settled.clone(),
    };
    let record: Arc<dyn TelemetrySpan + Send + Sync> = Arc::new(handle);
    let child = InMemoryChildSpan {
        record: record.clone(),
        state: state.clone(),
        index,
    };
    let handle = SpanHandle {
        inner: SpanInner::InMemory(child),
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let span: &SpanHandle = &handle;
        callback(span)
    }));
    settled.store(true, Ordering::SeqCst);
    match result {
        Ok(result) => {
            settle_span(&mut state.lock().unwrap(), index, false, None);
            result
        }
        Err(panic) => {
            settle_panicked_span(&mut state.lock().unwrap(), index);
            std::panic::resume_unwind(panic)
        }
    }
}

/// Async variant of [`start_span_with_parent`]. The callback future is
/// awaited before settlement, and an unwind still marks the span as an
/// automatic error before the original panic resumes.
async fn start_span_with_parent_async<T, F, Fut>(
    state: &Arc<Mutex<InMemoryTelemetryState>>,
    parent_id: Option<u64>,
    options: SpanOptions,
    callback: F,
) -> T
where
    F: FnOnce(SpanHandle) -> Fut,
    Fut: Future<Output = T>,
{
    let mut guard = state.lock().unwrap();
    let id = guard.next_span_id;
    guard.next_span_id += 1;
    let index = guard.spans.len();
    guard.spans.push(MutableRecordedTelemetrySpan {
        id,
        parent_id,
        name: options.name,
        attributes: copy_attributes(options.attributes.as_ref()),
        ..Default::default()
    });
    drop(guard);

    let settled = Arc::new(AtomicBool::new(false));
    let handle = InMemorySpanHandle {
        state: state.clone(),
        index,
        settled: settled.clone(),
    };
    let record: Arc<dyn TelemetrySpan + Send + Sync> = Arc::new(handle);
    let child = InMemoryChildSpan {
        record,
        state: state.clone(),
        index,
    };
    let span = SpanHandle {
        inner: SpanInner::InMemory(child),
    };

    let result = AssertUnwindSafe(callback(span)).catch_unwind().await;
    settled.store(true, Ordering::SeqCst);
    match result {
        Ok(result) => {
            settle_span(&mut state.lock().unwrap(), index, false, None);
            result
        }
        Err(panic) => {
            settle_panicked_span(&mut state.lock().unwrap(), index);
            std::panic::resume_unwind(panic)
        }
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
        F: FnOnce(&SpanHandle) -> T,
    {
        start_span_with_parent(&self.state, None, options, callback)
    }

    /// Start a root span whose lifetime follows an async callback.
    pub async fn start_span_async<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(SpanHandle) -> Fut,
        Fut: Future<Output = T>,
    {
        start_span_with_parent_async(&self.state, None, options, callback).await
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
        F: FnOnce(&SpanHandle) -> T,
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
            SpanOptions {
                name: "n".into(),
                attributes: None,
            },
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
            SpanOptions {
                name: "root".into(),
                attributes: None,
            },
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
    fn memory_records_nested_spans_with_parent_id() {
        let ctx = InMemoryTelemetryContext::new();
        ctx.start_span(
            SpanOptions {
                name: "root".into(),
                attributes: None,
            },
            |root| {
                root.start_span(
                    SpanOptions {
                        name: "child".into(),
                        attributes: None,
                    },
                    |child| {
                        child.start_span(
                            SpanOptions {
                                name: "grandchild".into(),
                                attributes: None,
                            },
                            |_gc| {},
                        );
                    },
                );
            },
        );
        let spans = ctx.get_spans();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].name, "root");
        assert_eq!(spans[0].parent_id, None);
        assert_eq!(spans[1].name, "child");
        assert_eq!(spans[1].parent_id, Some(spans[0].id));
        assert_eq!(spans[2].name, "grandchild");
        assert_eq!(spans[2].parent_id, Some(spans[1].id));
        assert!(spans[0].settled && spans[1].settled && spans[2].settled);
        // Settle order is innermost-first: root picks up the highest sequence.
        assert!(spans[2].end_sequence.unwrap() < spans[1].end_sequence.unwrap());
        assert!(spans[1].end_sequence.unwrap() < spans[0].end_sequence.unwrap());
        assert_eq!(
            spans[2].end_sequence.unwrap() + 2,
            spans[0].end_sequence.unwrap()
        );
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

    #[test]
    fn panic_settles_span_and_preserves_panic() {
        let ctx = InMemoryTelemetryContext::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.start_span(
                SpanOptions {
                    name: "panic".into(),
                    attributes: None,
                },
                |span| {
                    span.add_event("before_panic", None);
                    panic!("callback failed");
                },
            );
        }));

        assert!(panic.is_err());
        let span = &ctx.get_spans()[0];
        assert!(span.settled);
        assert_eq!(span.status, SpanStatus::Error { error: None });
        assert!(span.end_sequence.is_some());
        assert_eq!(span.events.len(), 1);
    }

    #[test]
    fn panic_keeps_explicit_status_and_settles_nested_spans_in_order() {
        let ctx = InMemoryTelemetryContext::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.start_span(
                SpanOptions {
                    name: "parent".into(),
                    attributes: None,
                },
                |parent| {
                    parent.set_status(SpanStatus::Ok);
                    parent.start_span(
                        SpanOptions {
                            name: "child".into(),
                            attributes: None,
                        },
                        |_child| {
                            panic!("nested callback failed");
                        },
                    );
                },
            );
        }));

        assert!(panic.is_err());
        let spans = ctx.get_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].status, SpanStatus::Ok);
        assert_eq!(spans[1].status, SpanStatus::Error { error: None });
        assert!(spans[1].end_sequence.unwrap() < spans[0].end_sequence.unwrap());
    }
}
