//! Harness events — port of `packages/agent/src/harness/events.ts`.
//!
//! `run_start` / `run_end` lifecycle events plus the typeless
//! `HarnessEventBus` with per-type subscriptions and watch handles
//! (buffering until `start`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

/// `run_start` lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStartEvent {
    pub lane: String,
    pub run_id: String,
}

/// `run_end` lifecycle event carrying the run outcome and the leaf entry id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEndEvent {
    pub lane: String,
    pub run_id: String,
    pub outcome: RunOutcome,
    pub leaf_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed,
}

impl RunOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Aborted => "aborted",
            RunOutcome::Failed => "failed",
        }
    }
}

impl std::str::FromStr for RunOutcome {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "completed" => Ok(RunOutcome::Completed),
            "aborted" => Ok(RunOutcome::Aborted),
            "failed" => Ok(RunOutcome::Failed),
            _ => Err(()),
        }
    }
}

/// The `HarnessEvent` union, mirroring upstream `RunStartEvent | RunEndEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessEvent {
    RunStart(RunStartEvent),
    RunEnd(RunEndEvent),
}

impl HarnessEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            HarnessEvent::RunStart(_) => "run_start",
            HarnessEvent::RunEnd(_) => "run_end",
        }
    }

    pub fn as_run_start(&self) -> Option<&RunStartEvent> {
        match self {
            HarnessEvent::RunStart(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_run_end(&self) -> Option<&RunEndEvent> {
        match self {
            HarnessEvent::RunEnd(e) => Some(e),
            _ => None,
        }
    }
}

/// Listener callback for harness events. Async listeners are fire-and-forget
/// (matching upstream `void listener(event)` without awaiting).
pub type HarnessEventListener = Box<dyn Fn(&HarnessEvent) + Send + Sync>;

/// Shared watch state: while a live listener is set events are delivered
/// synchronously; otherwise they accumulate in the buffer (mirroring
/// upstream's buffer-until-`start` behavior).
#[derive(Default)]
struct WatchState {
    live: Mutex<Option<HarnessEventListener>>,
    buffered: Mutex<Vec<HarnessEvent>>,
}

impl WatchState {
    fn deliver(&self, event: &HarnessEvent) {
        if let Some(listener) = self
            .live
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            listener(event);
        } else {
            self.buffered
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(event.clone());
        }
    }

    fn start(&self, listener: HarnessEventListener) {
        // Stay in buffering mode while flushing so reentrant emissions
        // preserve order (port of the upstream start loop).
        loop {
            let pending = std::mem::take(
                &mut *self
                    .buffered
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
            if pending.is_empty() {
                break;
            }
            for event in &pending {
                listener(event);
            }
        }
        *self.live.lock().unwrap_or_else(|error| error.into_inner()) = Some(listener);
    }

    fn clear(&self) {
        self.buffered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.live
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

/// A watch handle: snapshot plus event subscription that buffers until
/// `start` is called — port of upstream `WatchHandle<TSnapshot>`.
pub struct WatchHandle<T> {
    pub snapshot: T,
    state: Arc<WatchState>,
}

impl<T> WatchHandle<T> {
    /// Switch from buffering mode to live delivery. Pending buffered events
    /// are flushed first; reentrant emissions during the flush preserve order.
    pub fn start(&self, listener: HarnessEventListener) {
        self.state.start(listener);
    }

    /// Unsubscribe and drop any buffered events. The bus entry self-cleans
    /// on the next emit (the bus only holds a weak reference).
    pub fn unsubscribe(&self) {
        self.state.clear();
    }

    pub fn is_listening(&self) -> bool {
        self.state
            .live
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }
}

/// Typeless harness event bus — port of upstream `HarnessEventBus`.
#[derive(Default)]
pub struct HarnessEventBus {
    listeners: HashMap<&'static str, Vec<(usize, HarnessEventListener)>>,
    watches: HashMap<usize, Weak<WatchState>>,
    next_id: usize,
}

impl HarnessEventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a listener for a single event type and return a subscription
    /// id usable with [`HarnessEventBus::unsubscribe`]. Earlier events are
    /// not replayed (no event buffer).
    pub fn on(&mut self, event_type: &'static str, listener: HarnessEventListener) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.listeners
            .entry(event_type)
            .or_default()
            .push((id, listener));
        id
    }

    /// Unsubscribe a listener registered with [`HarnessEventBus::on`].
    pub fn unsubscribe(&mut self, id: usize) {
        for listeners in self.listeners.values_mut() {
            listeners.retain(|(sid, _)| *sid != id);
        }
        self.listeners.retain(|_, v| !v.is_empty());
    }

    /// Publish an event to direct subscriptions for its type and to every
    /// watcher (buffered or live). Async listener results are not awaited.
    pub fn emit(&self, event: &HarnessEvent) {
        let event_type = event.event_type();
        if let Some(listeners) = self.listeners.get(event_type) {
            for (_, listener) in listeners {
                listener(event);
            }
        }
        for (token, state) in &self.watches {
            if let Some(state) = state.upgrade() {
                let _ = token;
                state.deliver(event);
            }
        }
    }

    /// Create a watch handle over a snapshot. Events are buffered until the
    /// handle's `start` is called (upstream `watch(captureSnapshot)`).
    pub fn watch<T>(&mut self, snapshot: T) -> WatchHandle<T> {
        let state = Arc::new(WatchState::default());
        let token = self.next_id;
        self.next_id += 1;
        self.watches.insert(token, Arc::downgrade(&state));
        WatchHandle { snapshot, state }
    }

    /// Number of subscribed listeners for one event type (test helper).
    pub fn listener_count(&self, event_type: &str) -> usize {
        self.listeners.get(event_type).map(|v| v.len()).unwrap_or(0)
    }

    /// Number of live watch handles (stale handles are pruned lazily on emit;
    /// this helper also prunes so the number is accurate).
    pub fn watch_count(&self) -> usize {
        self.watches
            .values()
            .filter(|w| w.strong_count() > 0)
            .count()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn run_start(lane: &str, run_id: &str) -> HarnessEvent {
        HarnessEvent::RunStart(RunStartEvent {
            lane: lane.into(),
            run_id: run_id.into(),
        })
    }

    fn run_end(lane: &str, run_id: &str, outcome: RunOutcome, leaf_id: &str) -> HarnessEvent {
        HarnessEvent::RunEnd(RunEndEvent {
            lane: lane.into(),
            run_id: run_id.into(),
            outcome,
            leaf_id: leaf_id.into(),
        })
    }

    #[test]
    fn event_types_and_outcome_strings() {
        assert_eq!(run_start("l", "r").event_type(), "run_start");
        assert_eq!(
            run_end("l", "r", RunOutcome::Completed, "e").event_type(),
            "run_end"
        );
        assert_eq!(RunOutcome::Completed.as_str(), "completed");
        assert_eq!(RunOutcome::Aborted.as_str(), "aborted");
        assert_eq!(RunOutcome::Failed.as_str(), "failed");
        assert_eq!(
            "completed".parse::<RunOutcome>().unwrap(),
            RunOutcome::Completed
        );
        assert!("bogus".parse::<RunOutcome>().is_err());
    }

    #[test]
    fn on_delivers_only_matching_type_and_unsubscribes() {
        let mut bus = HarnessEventBus::new();
        let received = Arc::new(Mutex::new(Vec::<String>::new()));
        let rx = received.clone();
        let sub = bus.on(
            "run_start",
            Box::new(move |e| {
                rx.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(e.event_type().to_string());
            }),
        );
        bus.emit(&run_start("l", "r"));
        bus.emit(&run_end("l", "r", RunOutcome::Completed, "e"));
        assert_eq!(
            *received.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["run_start".to_string()]
        );
        assert_eq!(bus.listener_count("run_start"), 1);
        bus.unsubscribe(sub);
        assert_eq!(bus.listener_count("run_start"), 0);
        bus.emit(&run_start("l", "r"));
        assert_eq!(
            received
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
    }

    #[test]
    fn watch_buffers_until_start_then_delivers_live() {
        let mut bus = HarnessEventBus::new();
        bus.emit(&run_start("l", "r1"));
        let handle = bus.watch(42i32);
        assert_eq!(handle.snapshot, 42);
        // Watch created after r1: no replay (upstream has no replayed events).
        bus.emit(&run_start("l", "r2"));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sx = seen.clone();
        handle.start(Box::new(move |e| {
            sx.lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(e.event_type().to_string());
        }));
        assert_eq!(
            *seen.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["run_start".to_string()],
            "buffered run_start delivered after start"
        );

        bus.emit(&run_end("l", "r2", RunOutcome::Completed, "e"));
        assert_eq!(
            *seen.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["run_start".to_string(), "run_end".to_string()]
        );
        assert!(handle.is_listening());

        handle.unsubscribe();
        bus.emit(&run_start("l", "r3"));
        assert_eq!(
            seen.lock().unwrap_or_else(|error| error.into_inner()).len(),
            2,
            "no delivery after unsubscribe"
        );
    }

    #[test]
    fn watch_handle_drop_stops_delivery() {
        let mut bus = HarnessEventBus::new();
        let handle = bus.watch(());
        let seen = Arc::new(Mutex::new(0usize));
        let sx = seen.clone();
        handle.start(Box::new(move |_| {
            *sx.lock().unwrap_or_else(|error| error.into_inner()) += 1;
        }));
        bus.emit(&run_start("l", "r"));
        assert_eq!(*seen.lock().unwrap_or_else(|error| error.into_inner()), 1);
        drop(handle);
        bus.emit(&run_start("l", "r2"));
        assert_eq!(
            *seen.lock().unwrap_or_else(|error| error.into_inner()),
            1,
            "stale handle no longer receives"
        );
    }
}
