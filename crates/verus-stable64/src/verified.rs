//! Verus-verified implementation of the absolute-target growth leaf.

use vstd::prelude::*;

verus! {

use verus_ic::stable64::max_pages;
use verus_ic::token;

/// Grow stable memory to at least `target_pages` pages and return the final page
/// count.
///
/// `token::grow` traps on a subnet refusal, so success implies the post-state
/// has at least `target_pages` pages; the contract exposes that guarantee.
pub fn grow_to(target_pages: u64) -> (final_pages: u64)
    requires
        target_pages as int <= max_pages(),
    ensures
        final_pages >= target_pages,
{
    let (pages, Tracked(mut token)) = token::open(());
    if pages >= target_pages {
        return pages;
    }
    let additional = target_pages - pages;
    proof {
        assert(pages as int + additional as int == target_pages as int);
    }
    token::grow(Tracked(&mut token), additional);
    target_pages
}

} // verus!
