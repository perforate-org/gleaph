//! Hasher plumbing.
//!
//! Every crate-owned map defaults to [`DefaultBuildHasher`]
//! (`rapidhash::fast::RandomState`). The keys hashed here are internal
//! identities and grid cells, never adversarial input, and
//! `benches/paint_bench.rs` measures the difference against SipHash directly
//! on the per-frame paint path. Public types that own a hash map keep an
//! `S: BuildHasher` type parameter so callers can still choose another
//! hasher; the aliases below keep the many internal sites terse while the
//! default lives here alone.

/// The build-hasher every gpui-graph map defaults to.
pub type DefaultBuildHasher = rapidhash::fast::RandomState;

/// `std::collections::HashMap` with a defaulted build-hasher type parameter.
pub(crate) type HashMap<K, V, S = DefaultBuildHasher> = std::collections::HashMap<K, V, S>;
