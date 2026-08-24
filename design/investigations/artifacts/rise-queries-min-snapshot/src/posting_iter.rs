//! The two index traits the RISE query operators are generic over, copied verbatim from
//! upstream `indexes/mod.rs` with one deviation: upstream declares
//! `trait InvertedIndex: MemSize + MemDbg` (mem_dbg). The operator bodies never use those
//! capabilities, so the supertrait is dropped here to keep the extraction dependency-free.
//! Restoring it is mechanical if an internal crate wants `mem_size` reporting.

/// Upstream `PostingListIter` (verbatim contract):
/// a cursor over one ascending posting list. `current_doc()` reports the frontier docid;
/// past-the-end it reports the universe size (`n_docs()`), not a sentinel max — the
/// operators compare against `idx.n_docs()` to detect exhaustion.
pub trait PostingListIter {
    fn current_doc(&self) -> u64;
    fn current_pos(&self) -> usize;
    fn next_geq(&mut self, lower_bound: u64);
    fn next_doc(&mut self);
    fn freq(&mut self) -> u64;
    fn len(&self) -> usize;
}

/// Upstream `InvertedIndex` (minus the mem_dbg supertrait).
pub trait InvertedIndex {
    type IterType<'a>: PostingListIter
    where
        Self: 'a;

    fn n_docs(&self) -> usize;
    fn n_terms(&self) -> usize;
    fn get_plist_iter(&self, i: usize) -> Self::IterType<'_>;
}
