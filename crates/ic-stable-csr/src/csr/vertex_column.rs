//! Random-access vertex column (`M_v`) for CSR (paired with DGAP `M_e`) without full `Vec<V>` snapshots.
//!
//! When backed by [`StableVec`], the bytes at the start of `M_v` are **`ic_stable_structures::vec::Vec` V1**
//! (magic `SVC`, 64-byte header, then element slots — see `ic_stable_structures::base_vec` module documentation).

use ic_stable_structures::Memory;
use ic_stable_structures::vec::Vec as StableVec;
use ic_stable_vec_deque::VecDeque as StableVecDeque;

use crate::memory_util::GrowFailed;
use crate::traits::CsrVertex;

/// Stable-memory vertex column: `len` / `get` / `set` by logical index.
pub trait CsrVertexColumn<V: CsrVertex> {
    fn col_len(&self) -> u64;
    fn col_get(&self, i: u64) -> Option<V>;
    fn col_set(&self, i: u64, v: V);

    /// Append one row at the end. [`StableVec`] reports grow failure via panic; deque returns [`GrowFailed`].
    fn col_push_back(&self, v: &V) -> Result<(), GrowFailed>;

    /// Remove and return the last row, if any.
    fn col_pop_back(&self) -> Option<V>;
}

impl<V, M> CsrVertexColumn<V> for StableVec<V, M>
where
    V: CsrVertex,
    M: Memory,
{
    fn col_len(&self) -> u64 {
        StableVec::len(self)
    }

    fn col_get(&self, i: u64) -> Option<V> {
        StableVec::get(self, i)
    }

    fn col_set(&self, i: u64, v: V) {
        StableVec::set(self, i, &v);
    }

    fn col_push_back(&self, v: &V) -> Result<(), GrowFailed> {
        StableVec::push(self, v);
        Ok(())
    }

    fn col_pop_back(&self) -> Option<V> {
        StableVec::pop(self)
    }
}

impl<V, M> CsrVertexColumn<V> for StableVecDeque<V, M>
where
    V: CsrVertex,
    M: Memory,
{
    fn col_len(&self) -> u64 {
        self.len()
    }

    fn col_get(&self, i: u64) -> Option<V> {
        self.get(i)
    }

    fn col_set(&self, i: u64, v: V) {
        StableVecDeque::set(self, i, &v);
    }

    fn col_push_back(&self, v: &V) -> Result<(), GrowFailed> {
        StableVecDeque::push_back(self, v).map_err(Into::into)
    }

    fn col_pop_back(&self) -> Option<V> {
        StableVecDeque::pop_back(self)
    }
}
