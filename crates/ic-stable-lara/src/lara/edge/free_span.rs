//! Stable LARA free span metadata stores.
//!
//! Free spans are retired physical edge ranges that update and maintenance code
//! can reuse. Clean query scans must not read these stores.

pub mod array;
#[cfg(feature = "canbench")]
pub mod bench;
pub mod binned;
pub mod dual_index;
pub mod store;

pub use array::FreeSpanArrayStore;
pub use binned::{
    BIN_COUNT, FreeSpanBinnedBTreeStore, FreeSpanBinnedError, FreeSpanBinnedPagedStore,
    FreeSpanBinnedStore, InitError as FreeSpanBinnedInitError, SpanId, size_class,
};
pub use dual_index::{
    FreeSpanDualIndexError, FreeSpanDualIndexStore, LenStartKey, SpanLen, StartKey,
};
pub use store::{FreeSpan, FreeSpanKey, FreeSpanStore};
