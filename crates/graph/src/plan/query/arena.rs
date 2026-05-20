//! Per-query slot buffer pool for indexed [`super::row::PlanRow`] execution.
//!
//! Reset at the start of each [`super::execute_plan_query_bindings`] call. After hash-join
//! (and similar) steps, intermediate row batches are recycled so later allocations can
//! reuse `Vec` capacity within the same query.

use std::cell::RefCell;

use super::row::PlanRow;

const MAX_SLOT_POOL: usize = 32;
const MIN_RECYCLE_SLOT_CAPACITY: usize = 4;

thread_local! {
    static QUERY_ARENA: RefCell<QueryArena> = RefCell::new(QueryArena::new());
}

/// Slot `Vec` pool scoped to one plan-query execution.
pub(crate) struct QueryArena {
    slot_pool: Vec<Vec<Option<super::executor::PlanBinding>>>,
}

impl QueryArena {
    pub fn new() -> Self {
        Self {
            slot_pool: Vec::new(),
        }
    }

    pub fn with<R>(f: impl FnOnce(&mut Self) -> R) -> R {
        QUERY_ARENA.with(|arena| f(&mut arena.borrow_mut()))
    }

    pub fn reset(&mut self) {
        self.slot_pool.clear();
    }

    pub fn recycle_rows(&mut self, rows: Vec<PlanRow>) {
        for mut row in rows {
            if row.layout().is_some() {
                let slots = row.take_slots();
                self.recycle_slots_buffer(slots);
            }
        }
    }

    fn recycle_slots_buffer(&mut self, slots: Vec<Option<super::executor::PlanBinding>>) {
        if slots.capacity() < MIN_RECYCLE_SLOT_CAPACITY || self.slot_pool.len() >= MAX_SLOT_POOL {
            return;
        }
        self.slot_pool.push(slots);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_planner::BindingLayout;
    use ic_stable_lara::VertexId;
    use std::rc::Rc;

    use super::super::executor::PlanBinding;

    #[test]
    fn recycle_rows_buffers_slot_vecs() {
        QueryArena::with(|arena| {
            arena.reset();
            let row = PlanRow::with_layout_and_binding(
                Rc::new(BindingLayout::single("a".into())),
                "a",
                PlanBinding::Vertex(VertexId::from(1)),
            );
            arena.recycle_rows(vec![row]);
            assert_eq!(arena.slot_pool.len(), 1);
        });
    }
}
