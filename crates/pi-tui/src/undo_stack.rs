//! Generic undo stack with clone-on-push semantics — port of
//! `packages/tui/src/undo-stack.ts`.
//!
//! Stores clones of state snapshots. Popped snapshots are returned directly
//! (no re-cloning) since they are already detached.

/// A stack of state snapshots.
#[derive(Debug, Default, Clone)]
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S> UndoStack<S> {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Push a clone of the given state onto the stack.
    pub fn push(&mut self, state: S) {
        self.stack.push(state);
    }

    /// Pop and return the most recent snapshot.
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }

    /// Remove all snapshots.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_roundtrip() {
        let mut stack = UndoStack::new();
        stack.push(1);
        stack.push(2);
        assert_eq!(stack.pop(), Some(2));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn clear_removes_all() {
        let mut stack = UndoStack::new();
        stack.push(1);
        stack.push(2);
        stack.clear();
        assert_eq!(stack.len(), 0);
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn snapshots_are_detached_clones() {
        let mut stack = UndoStack::new();
        let v = vec![1, 2, 3];
        stack.push(v.clone());
        let mut popped = stack.pop().unwrap();
        popped.push(4);
        // The original must be unchanged.
        assert_eq!(v, vec![1, 2, 3]);
        assert_eq!(popped, vec![1, 2, 3, 4]);
    }
}
