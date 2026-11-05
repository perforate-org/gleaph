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

/// The share of a leaf that a bucket's weight entitles it to.
///
/// The rebalance direction starts from the leaf and divides it; `content_capacity` then says how much
/// content that share holds. Rounding is down, so the parts never exceed the whole.
pub(crate) fn share_of(leaf: SpanSlots, weight: u32, total_weight: u32) -> SpanSlots {
    if total_weight == 0 {
        return SpanSlots(0);
    }
    SpanSlots(leaf.0.saturating_mul(u64::from(weight)) / u64::from(total_weight))
}

/// What a leaf sized for `content` can hold after a rebalance, exercising both directions.
///
/// The two must agree: grow sizes a leaf for some content, rebalance divides that leaf back and asks how much
/// each bucket may hold. If rebalance answered less for the same content, every grow would be immediately
/// undone by the next rebalance - the oscillation the demotion hysteresis tests guard against on the mode
/// axis, here on the capacity axis.
#[cfg(test)]
pub(crate) fn rebalance_holds(
    leaf: SpanSlots,
    content: ContentSlots,
    weight: u32,
    total: u32,
) -> bool {
    content_capacity(share_of(leaf, weight, total)) >= content
}

/// How many rows a bucket can fold from its run into its span.
///
/// The bucket holds `resident` rows of which `prefix` already sit in the span; the rest are in the run. The
/// span's tail offers `free` slots. The fold is the smaller of the two - content the bucket does not have
/// cannot move, and content the tail cannot hold must wait for a wider region.
///
/// The caller still bounds this by the run's own head-drop capacity (a run's class may refuse to shrink);
/// this function only answers the arithmetic half, so that the row count is derived in one place instead of
/// being spelled out at each fold site.
pub(crate) fn foldable_rows(
    prefix: ContentSlots,
    resident: ContentSlots,
    free: SpanSlots,
) -> SpanSlots {
    let waiting = resident.0.saturating_sub(prefix.0);
    SpanSlots(u64::from(waiting).min(free.0))
}

/// What a bucket asks a leaf for, as one value.
///
/// The content is the mode-specific occupancy (rows for a slab prefix, root regions for a tree) and the
/// span is that content plus its gap. Keeping them in one place is what stops a caller from passing content
/// as if it were a span: the placement entries take spans, and the only way to obtain one from content is
/// [`SpanRequest::for_content`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpanRequest {
    content: ContentSlots,
    span: SpanSlots,
}

impl SpanRequest {
    pub(crate) fn for_content(content: ContentSlots) -> Self {
        Self {
            content,
            span: slots_for_content(content),
        }
    }

    pub(crate) const fn content(self) -> ContentSlots {
        self.content
    }

    pub(crate) const fn span(self) -> SpanSlots {
        self.span
    }

    /// Whether this request fits in `headroom` slots.
    ///
    /// The headroom is the mode cap minus what the bucket already holds, i.e. `compute_bucket_allocation`;
    /// asking for more is what promotion exists for.
    pub(crate) fn fits_in(self, headroom: SpanSlots) -> bool {
        self.span.0 <= headroom.0
    }
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

    /// A request carries the content it was built from and the span the gap rule derives, and it fits
    /// exactly when that span does.
    #[test]
    fn request_pairs_content_with_its_span() {
        for content in [0u32, 1, 8, 9, 80, 1_024, 4_096] {
            let request = SpanRequest::for_content(ContentSlots::new(content));
            assert_eq!(request.content(), ContentSlots::new(content));
            assert_eq!(
                request.span(),
                slots_for_content(ContentSlots::new(content))
            );
            assert!(request.fits_in(request.span()));
            if request.span().get() > 0 {
                assert!(
                    !request.fits_in(SpanSlots::new(request.span().get() - 1)),
                    "one slot short must not fit"
                );
            }
        }
    }

    /// A fold never moves more rows than the bucket holds outside its prefix, nor more than the tail holds.
    #[test]
    fn folds_are_bounded_by_waiting_rows_and_free_tail() {
        let cases = [
            // (prefix, resident, free) -> folded
            (0u32, 10u32, 100u64, 10u64), // tail is generous; only the waiting rows move
            (5, 10, 2, 2),                // tail is the tighter bound
            (5, 5, 100, 0),               // nothing waiting
            (10, 0, 100, 0),              // prefix beyond resident saturates to zero
            (0, 0, 0, 0),
        ];
        for (prefix, resident, free, expected) in cases {
            assert_eq!(
                foldable_rows(
                    ContentSlots::new(prefix),
                    ContentSlots::new(resident),
                    SpanSlots::new(free)
                )
                .0,
                expected,
                "prefix {prefix}, resident {resident}, free {free}"
            );
        }
    }

    /// A leaf grown for some content gives that content back when the leaf is divided again: a grow cannot be
    /// undone by the next rebalance.
    #[test]
    fn grow_and_rebalance_agree_on_the_same_content() {
        for content in [0u32, 1, 8, 9, 80, 1_024, 4_096] {
            let want = ContentSlots::new(content);
            let leaf = slots_for_content(want);
            assert!(
                rebalance_holds(leaf, want, 1, 1),
                "content {content} does not survive rebalance over its own leaf ({})",
                leaf.0
            );
        }
    }

    /// Splitting a leaf between weighted buckets keeps the sum of shares within the leaf.
    #[test]
    fn shares_never_exceed_the_leaf() {
        let leaf = SpanSlots::new(10_000);
        let weights = [1u32, 2, 3, 7, 100];
        let total: u32 = weights.iter().sum();
        let sum: u64 = weights.iter().map(|w| share_of(leaf, *w, total).0).sum();
        assert!(sum <= leaf.0, "shares {sum} exceed the leaf {}", leaf.0);
        assert_eq!(share_of(leaf, 0, total).0, 0, "a zero weight gets nothing");
        assert_eq!(
            share_of(leaf, 1, 0).0,
            0,
            "an empty weight set shares nothing"
        );
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
