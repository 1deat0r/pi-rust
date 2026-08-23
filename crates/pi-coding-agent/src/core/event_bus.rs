//! Typed event bus — port of `packages/coding-agent/src/core/event-bus.ts`.
//!
//! A minimal channel-based emitter used across the agent for extension
//! communication and internal pub/sub. Handler errors are caught and logged
//! (never propagated), matching the upstream `safeHandler`.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Event handler closure type.
pub type EventHandler = dyn Fn(Arc<dyn Any + Send + Sync>) + Send + Sync;

/// A subscribed handler. Handlers receive the event payload as a boxed value.
struct Registered {
    id: u64,
    handler: Arc<EventHandler>,
}

/// Event bus with `emit`/`on` and a `clear` controller operation.
///
/// Mirrors upstream `EventBusController`: `on` returns an unsubscribe closure,
/// `emit` is fire-and-forget, `clear` removes all listeners.
#[derive(Clone, Default)]
pub struct EventBus {
    handlers: Arc<Mutex<HashMap<String, Vec<Registered>>>>,
    next_id: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit an event payload on a channel (fire-and-forget).
    pub fn emit(&self, channel: &str, data: impl Any + Send + Sync) {
        let payload = Arc::new(data);
        let snapshot: Vec<Arc<EventHandler>> = {
            let guard = self.handlers.lock().unwrap();
            guard
                .get(channel)
                .map(|list| list.iter().map(|r| r.handler.clone()).collect())
                .unwrap_or_default()
        };
        for handler in snapshot {
            // Safe handler: errors are caught, never propagated to emit().
            let call = std::panic::AssertUnwindSafe(|| handler(payload.clone()));
            if std::panic::catch_unwind(call).is_err() {
                tracing::warn!(channel, "event handler panicked");
            }
        }
    }

    /// Subscribe to a channel. Returns an unsubscribe closure.
    pub fn on(
        &self,
        channel: &str,
        handler: Box<EventHandler>,
    ) -> Box<dyn Fn() + Send + Sync + '_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.handlers.lock().unwrap();
        guard
            .entry(channel.to_string())
            .or_default()
            .push(Registered {
                id,
                handler: Arc::new(handler),
            });
        let handlers = self.handlers.clone();
        let channel = channel.to_string();
        drop(guard);
        Box::new(move || {
            let mut guard = handlers.lock().unwrap();
            if let Some(list) = guard.get_mut(&channel) {
                list.retain(|registered| registered.id != id);
            }
        })
    }

    /// Remove all listeners on all channels.
    pub fn clear(&self) {
        self.handlers.lock().unwrap().clear();
    }

    /// Number of channels (test helper).
    pub fn channel_count(&self) -> usize {
        self.handlers.lock().unwrap().len()
    }

    /// Number of handlers on a channel (test helper).
    pub fn handler_count(&self, channel: &str) -> usize {
        self.handlers
            .lock()
            .unwrap()
            .get(channel)
            .map(|l| l.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_and_receives() {
        let bus = EventBus::new();
        let received = Arc::new(Mutex::new(Vec::new()));
        let got = received.clone();
        let _unsub = bus.on(
            "x",
            Box::new(move |data| {
                let value = data.downcast_ref::<u32>().copied().unwrap_or(0);
                got.lock().unwrap().push(value);
            }),
        );
        bus.emit("x", 42u32);
        bus.emit("x", 7u32);
        assert_eq!(*received.lock().unwrap(), vec![42, 7]);
    }

    #[test]
    fn unsubscribe_stops_delivery() {
        let bus = EventBus::new();
        let count = Arc::new(Mutex::new(0));
        let c = count.clone();
        let unsub = bus.on("c", Box::new(move |_| *c.lock().unwrap() += 1));
        bus.emit("c", ());
        unsub();
        bus.emit("c", ());
        assert_eq!(*count.lock().unwrap(), 1);
    }

    #[test]
    fn unsubscribe_only_removes_own_handler() {
        let bus = EventBus::new();
        let count = Arc::new(Mutex::new(0));
        let c1 = count.clone();
        let _unsub = bus.on("m", Box::new(move |_| *c1.lock().unwrap() += 1));
        let c2 = count.clone();
        let unsub = bus.on("m", Box::new(move |_| *c2.lock().unwrap() += 1));
        assert_eq!(bus.handler_count("m"), 2);
        unsub();
        assert_eq!(bus.handler_count("m"), 1);
        bus.emit("m", ());
        assert_eq!(*count.lock().unwrap(), 1);
    }

    #[test]
    fn clear_removes_all() {
        let bus = EventBus::new();
        let count = Arc::new(Mutex::new(0));
        let c = count.clone();
        let _unsub1 = bus.on("a", Box::new(move |_| *c.lock().unwrap() += 1));
        let c2 = count.clone();
        let _unsub2 = bus.on("b", Box::new(move |_| *c2.lock().unwrap() += 1));
        bus.clear();
        bus.emit("a", ());
        bus.emit("b", ());
        assert_eq!(*count.lock().unwrap(), 0);
        assert_eq!(bus.channel_count(), 0);
    }

    #[test]
    fn emit_does_not_propagate_handler_errors() {
        let bus = EventBus::new();
        let _unsub = bus.on("p", Box::new(|_| panic!("handler blew up")));
        // Must not panic.
        bus.emit("p", ());
    }
}
