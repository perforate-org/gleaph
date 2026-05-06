//! Stable LARA free span metadata stores.
//!
//! Free spans are retired physical edge ranges that update and maintenance code
//! can reuse. Clean query scans must not read these stores.

pub mod array;
pub mod dual_index;
pub mod store;

pub use array::FreeSpanArrayStore;
pub use dual_index::{
    FreeSpanDualIndexError, FreeSpanDualIndexStore, LenStartKey, SpanLen, StartKey,
};
pub use store::{FreeSpan, FreeSpanKey, FreeSpanStore};
