//! The one place that converts between what a bucket holds and what a leaf reserves.
//!
//! Two directions need this arithmetic and they must not disagree:
//! - **grow** asks "how many slots does this content need" (`slots_for_content`), and
//! - **rebalance** asks "how much content fits in this share of the leaf" (`content_capacity`).
//!
//! They are exact inverses on purpose. Forward rounds *up*, backward rounds *down*, so a leaf sized for
//! some content holds that content and no more - a backward answer can never exceed what the forward
//! direction reserved. If the pair were off by a rounding step in the other direction, a grow would be
//! immediately undone by the next rebalance and the two would oscillate.
//!
//! The types keep the two quantities apart: `ContentSlots` is what rows and roots occupy (`u32`, matching
//! the descriptor fields), `SpanSlots` is what a slice reserves (`u64`, matching slot addresses).

/// Slots occupied by a bucket's content: rows for a slab prefix, root regions for a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ContentSlots(u32);

/// Slots a leaf reserves for that content, including the region's gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SpanSlots(u64);

impl ContentSlots {
    pub(crate) const fn new(slots: u32) -> Self {
        Self(slots)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

impl SpanSlots {
    pub(crate) const fn new(slots: u64) -> Self {
        Self(slots)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// The gap a region carries on top of its content: 12.5%, at least one slot.
///
/// The one definition of that rule. Callers that need the numbers instead of the types use this and
/// `slots_for_content`; nothing else should spell the fraction out.
pub(crate) fn gap_slots(content: u64) -> u64 {
    (content / 8).max(1)
}

/// Slots a leaf must reserve to hold `content`, rounding up.
///
/// Empty content reserves nothing: a bucket with no rows owns no region at all, so the minimal gap
/// applies only to content that exists.
pub(crate) fn slots_for_content(content: ContentSlots) -> SpanSlots {
    let content = u64::from(content.0);
    if content == 0 {
        return SpanSlots(0);
    }
    SpanSlots(content.saturating_add(gap_slots(content)))
}

/// The largest content that fits in `span`, rounding down - the exact inverse of [`slots_for_content`].
///
/// Defined as "largest `c` with `slots_for_content(c) <= span`" rather than as a closed form, so the two
/// directions cannot drift: whatever arithmetic changes here, the round trip stays exact.
pub(crate) fn content_capacity(span: SpanSlots) -> ContentSlots {
    let span = span.0;
    if span == 0 {
        return ContentSlots(0);
    }
    // Content never exceeds the slot count, so this bounds the search.
    let mut low = 0u32;
    let mut high = u32::try_from(span.min(u64::from(u32::MAX))).unwrap_or(u32::MAX);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if slots_for_content(ContentSlots::new(mid)).0 <= span {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    ContentSlots::new(low)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the two directions exist for: a leaf grown for some content holds exactly that
    /// content, so a rebalance re-run over its own output does not move anything.
    #[test]
    fn content_survives_the_round_trip() {
        for content in 0..=100_000u32 {
            let span = slots_for_content(ContentSlots::new(content));
            assert_eq!(
                content_capacity(span),
                ContentSlots::new(content),
                "content {content} -> span {} -> content must come back",
                span.0
            );
        }
    }

    /// Backward never promises more than forward reserved: every span holds what it was sized for.
    #[test]
    fn capacity_never_exceeds_the_span() {
        for span in 0..=100_000u64 {
            let capacity = content_capacity(SpanSlots::new(span));
            assert!(
                slots_for_content(capacity).0 <= span,
                "span {span} offered capacity {} which needs {} slots",
                capacity.0,
                slots_for_content(capacity).0
            );
        }
    }

    /// Gaps grow monotonically, and a small content still gets one slot of gap.
    #[test]
    fn gap_is_monotone_and_at_least_one() {
        let mut previous = 0;
        for content in 0..=10_000u64 {
            let gap = gap_slots(content);
            assert!(gap >= 1, "content {content} must carry a gap");
            assert!(gap >= previous, "gap must not shrink as content grows");
            previous = gap;
        }
        assert_eq!(gap_slots(0), 1);
        assert_eq!(gap_slots(8), 1);
        assert_eq!(gap_slots(80), 10);
    }
}
