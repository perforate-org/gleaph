//! Plain (release/canister) implementation of the absolute-target growth leaf.

use verus_ic::bridge::{stable64_grow, stable64_size};

/// Grow stable memory to at least `target_pages` 64 KiB pages and return the
/// resulting page count.
///
/// Mirrors the verified token's behaviour: a refused grow aborts instead of
/// returning a stale count.
pub fn grow_to(target_pages: u64) -> u64 {
    let pages = stable64_size();
    if pages >= target_pages {
        return pages;
    }
    let additional = target_pages - pages;
    let old_pages = stable64_grow(additional);
    assert_ne!(
        old_pages,
        u64::MAX,
        "stable64_grow was refused; aborting instead of continuing"
    );
    target_pages
}
