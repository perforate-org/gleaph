//! Hasher plumbing.
//!
//! `gpui-graph` uses `std::hash::BuildHasher` to let callers choose the hash
//! function behind its `HashMap`/`HashSet`s (e.g. SipHash for the default, or a
//! faster non-cryptographic hasher such as `rapidhash`). Public types that own
//! a hash map take an `S: BuildHasher` type parameter; the aliases below keep
//! the many internal sites terse while keeping the default hasher in one place.

/// `std::collections::HashMap` with a defaulted build-hasher type parameter.
pub(crate) type HashMap<K, V, S = std::collections::hash_map::RandomState> =
    std::collections::HashMap<K, V, S>;

/// `std::collections::HashSet` with a defaulted build-hasher type parameter.
pub(crate) type HashSet<K, S = std::collections::hash_map::RandomState> =
    std::collections::HashSet<K, S>;
