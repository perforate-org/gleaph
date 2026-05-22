//! Out-edge visit window.

/// Applies `offset` / `limit` to a logical stream of outgoing edges (after raw / match filters).
/// Applies `offset` / `limit` to a logical stream of outgoing edges (after raw / match filters).
pub(crate) struct OutEdgeVisitWindow {
    skip: usize,
    take: Option<usize>,
}

impl OutEdgeVisitWindow {
    pub(crate) fn new(offset: Option<usize>, limit: Option<usize>) -> Self {
        Self {
            skip: offset.unwrap_or(0),
            take: limit,
        }
    }

    /// Visit `edge` if it falls inside the window. Returns `false` when the caller should stop
    /// traversing (limit reached).
    pub(crate) fn emit_edge<E, V>(&mut self, edge: E, visit: &mut V) -> bool
    where
        V: FnMut(E),
    {
        if self.skip > 0 {
            self.skip -= 1;
            return true;
        }
        if let Some(0) = self.take {
            return false;
        }
        visit(edge);
        if let Some(t) = self.take.as_mut() {
            *t -= 1;
            if *t == 0 {
                return false;
            }
        }
        true
    }
}
