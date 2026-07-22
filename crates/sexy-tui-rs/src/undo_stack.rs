/// Generic undo stack with clone-on-push semantics.
///
/// Stores deep clones of state snapshots. Popped snapshots are returned
/// directly (no re-cloning) since they are already detached.
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S: Clone> UndoStack<S> {
    pub fn new() -> Self {
        UndoStack { stack: Vec::new() }
    }

    /// Push a deep clone of the given state onto the stack.
    pub fn push(&mut self, state: &S) {
        self.stack.push(state.clone());
    }

    /// Pop and return the most recent snapshot, or None if empty.
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

impl<S: Clone> Default for UndoStack<S> {
    fn default() -> Self {
        Self::new()
    }
}
