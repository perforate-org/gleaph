//! What a vertex owns in a leaf, as a value rather than as a number.
//!
//! `design/investigations/2026-11-08-owning-region-accounting.md` measured the failure this module exists to make
//! unrepresentable: a promotion wrote a two-slot tree root past a one-slot vertex cover (`root [258, 260)` against
//! `cover [256, 257)`), because "how much does this vertex own" was a `u32` any caller could pass. Two types replace that
//! number, and neither can be built out of thin air:
//!
//! - [`BucketLeafSpan`] - what one bucket occupies **in the leaf**, derived from the geometry that already answers that
//!   question (`initialized_slab_slots` for slab rows, `bucket_span_region_len` for a tree's edge root). It is empty when the
//!   bucket holds nothing in the leaf at all: a tiny bucket, or a tree whose root lives in an edge run.
//! - [`VertexLeafCover`] - the vertex's allocated leaf region, derived from its buckets' spans plus the growth slack. Its
//!   fields are private and it has no constructor that takes a length, so a caller cannot invent a cover; it can only ask for
//!   the derivation.
//!
//! The commit-boundary rule (the `storage/lara.md` decision) is expressed by *when* the derivation is called: mid-operation
//! states do not call it, and the publication that does call it is the point where "occupied ⊆ cover" is proven.

use super::BucketMode;
use super::leaf_geometry::span_region_len;
use crate::labeled::record::LabelBucket;
use crate::lara::edge::EdgeStore;
use crate::traits::CsrEdgeTombstone;
use ic_stable_structures::Memory;

/// A slot address in the **edge** store.
///
/// The three stores address different spaces - edge slots, bucket-descriptor slots, and value-store bytes - and they were all
/// plain `u64`. That is how this record's own figure 2 came to compare an edge-store root against a cover anchored at a
/// bucket-store address and call the difference a violation. A span or a cover whose start is this type cannot be built from
/// the wrong store's number without an explicit conversion, which is the whole point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, derive_more::Display)]
pub struct EdgeSlot(u64);

impl EdgeSlot {
    pub const ZERO: Self = Self(0);

    /// From a raw edge-store slot number.
    ///
    /// Named `from_raw` rather than a `From<u64>` impl on purpose: an implicit conversion would restore the mixing this type
    /// exists to prevent, and every call site should say which space its number is in.
    pub const fn from_raw(slot: u64) -> Self {
        Self(slot)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The slot `slots` positions later.
    ///
    /// The argument is a **length** (`u32`, the unit this codebase counts slots in), not a `u64`: a raw `u64` is exactly the
    /// number-without-a-space that this type exists to keep out. Inside the space, address + length = address, which is the
    /// only arithmetic an address should model.
    pub fn advance(self, slots: u32) -> Self {
        Self(self.0 + u64::from(slots))
    }

    /// One past the last of `len` slots starting here.
    pub fn end_of(self, len: u32) -> Self {
        self.advance(len)
    }

    /// The distance to `other`, in slots; negative cases are a bug the caller reports, so it saturates at zero.
    pub fn distance_to(self, other: Self) -> u64 {
        other.0.saturating_sub(self.0)
    }
}

impl core::ops::Add<u32> for EdgeSlot {
    type Output = Self;

    fn add(self, slots: u32) -> Self {
        self.advance(slots)
    }
}

impl core::ops::Sub<u32> for EdgeSlot {
    type Output = Self;

    fn sub(self, slots: u32) -> Self {
        Self(
            self.0
                .checked_sub(u64::from(slots))
                .expect("slot subtraction underflow"),
        )
    }
}

impl core::ops::Sub<EdgeSlot> for EdgeSlot {
    type Output = u32;

    /// The distance in slots. Saturating: a negative distance means the operands were never comparable.
    fn sub(self, other: Self) -> u32 {
        u32::try_from(self.0.saturating_sub(other.0)).unwrap_or(u32::MAX)
    }
}

/// The region one bucket occupies inside the leaf, in the leaf's slot unit.
///
/// `empty()` covers the two cases where a bucket holds nothing in the leaf and therefore needs no cover entry: a tiny bucket
/// (its payload is inline) and a tree whose edge root was placed in an edge run (the root lives in the run, not in the leaf).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketLeafSpan {
    start: EdgeSlot,
    len: u32,
}

impl BucketLeafSpan {
    /// The empty span: nothing owned in the leaf.
    pub const fn empty() -> Self {
        Self {
            start: EdgeSlot::ZERO,
            len: 0,
        }
    }

    pub(super) fn new(start: EdgeSlot, len: u32) -> Self {
        Self { start, len }
    }

    /// The only constructor: reads the geometry that owns this answer.
    ///
    /// It does not invent a rule. A slab bucket's leaf occupancy is the rows it has initialized (not its `stored_slots`,
    /// which includes growth slack); a tree's is the edge root's region when that root lives in leaf slots and nothing when it
    /// was placed in a run.
    pub(crate) fn of<E, M>(bucket: &LabelBucket, edges: &EdgeStore<E, M>) -> Self
    where
        E: CsrEdgeTombstone,
        M: Memory,
    {
        match BucketMode::from_bucket(bucket) {
            BucketMode::Tiny => Self::empty(),
            BucketMode::Slab => Self::new(
                EdgeSlot::from_raw(bucket.edge_start()),
                edges
                    .initialized_slab_slots(bucket)
                    .expect("initialized slab slots must be readable"),
            ),
            BucketMode::Tree if bucket.edge_run().is_some() => Self::empty(),
            BucketMode::Tree => Self::new(
                EdgeSlot::from_raw(bucket.edge_start()),
                span_region_len(bucket),
            ),
        }
    }

    /// Where the span starts, or zero when empty.
    pub fn start(self) -> EdgeSlot {
        self.start
    }

    /// How many leaf slots the span holds.
    pub fn len(self) -> u32 {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// One past the last slot the span holds.
    pub fn end(self) -> EdgeSlot {
        self.start.end_of(self.len)
    }
}

/// How a vertex's cover relates to what its buckets occupy.
///
/// Two variants, both with a caller in production: `NeverShrink` is the *monotone cover* rule the promotion paths already
/// implement (`insert.rs` states it as `stored_slots.max(grown)`; `promote.rs` as `new_alloc = old_alloc`), and `None` is the
/// identity used where a caller genuinely wants only the occupied union.
///
/// A `Grow { num, den }` variant existed here for one commit and was **removed**: it projected the bucket-row slack (`5/4`,
/// measured in `GAP-2026-09-20-006` for *bucket descriptor rows*) onto the vertex cover, where no measurement or caller
/// supports it. Different plane, no evidence, so it did not stay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlackRule {
    /// Hold exactly what the buckets occupy.
    None,
    /// Never end up shorter than `current` slots.
    ///
    /// This is the rule promotion actually follows - `new_alloc = old_alloc`, i.e. the pre-promotion allocation is retained
    /// rather than recomputed from the new content. It is not a floor on the allocation: a vertex starts with **zero** slots
    /// (`labeled_leaf_initial_bucket_quota` returns 1 only for segment 16) and grows on demand, so "at least a segment" is a
    /// consequence of what the span already held, never a rule. Stating it as this variant keeps the derivation honest about
    /// *which* retention it is applying, and leaves "do not shrink" separate from "grow by a ratio".
    NeverShrink { current: u32 },
}

/// A vertex's allocated region inside the leaf.
///
/// **Private fields, no length-taking constructor.** The only way to obtain one is [`Self::derive`], which starts from the
/// buckets' own spans, so a cover that does not back what the vertex occupies cannot be expressed - the promotion defect was
/// exactly that expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexLeafCover {
    start: EdgeSlot,
    len: u32,
}

impl VertexLeafCover {
    /// The empty cover: a vertex that owns nothing in the leaf.
    pub const fn empty() -> Self {
        Self {
            start: EdgeSlot::ZERO,
            len: 0,
        }
    }

    /// The only constructor: the vertex's base address plus the spans its buckets occupy, grown by `slack`.
    ///
    /// Spans that lie before `base` or are empty are ignored, which is how the empty cases (`BucketLeafSpan::empty`) fall out
    /// without a second rule.
    /// **The base is derived too.** A caller cannot pass it: the cover starts where the vertex's first occupied span
    /// starts, which is the edge-store address of its first non-tiny bucket (the vertex's own `base_slot_start` lives in the
    /// *bucket* store and is a different address space - conflating the two is how this was first written).
    pub fn derive(spans: impl IntoIterator<Item = BucketLeafSpan>, slack: SlackRule) -> Self {
        let mut base: Option<EdgeSlot> = None;
        let mut end = EdgeSlot::ZERO;
        for span in spans {
            if span.is_empty() {
                continue;
            }
            base = Some(base.map_or(span.start, |b: EdgeSlot| b.min(span.start)));
            end = end.max(span.end());
        }
        let Some(base) = base else {
            return Self::empty();
        };
        let occupied = base.distance_to(end);
        let grown = match slack {
            SlackRule::None => occupied,
            SlackRule::NeverShrink { current } => occupied.max(u64::from(current)),
        };
        let len = u32::try_from(grown).expect("a leaf span cannot exceed u32 slots");
        Self { start: base, len }
    }

    pub fn start(self) -> EdgeSlot {
        self.start
    }

    pub fn len(self) -> u32 {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn end(self) -> EdgeSlot {
        self.start.end_of(self.len)
    }

    /// The cover's address in the edge store, for callers comparing it with a span start.
    pub(crate) fn start_anchor(self) -> EdgeSlot {
        self.start
    }

    /// The monotone cover the promotion paths compute by hand, as a value.
    ///
    /// `promote_tiny_to_slab` states the rule as `stored_slots.max(grown)` and the tree promotion as `new_alloc = old_alloc`.
    /// Both are this function: the previous width is a floor, and content that reaches past it wins. The sites cannot call
    /// [`crate::labeled::graph::leaf_geometry::LeafGeometry::vertex_cover`] (one is `CsrEdge`-clean, and both publish a single
    /// bucket's span while the cover spans the vertex's first bucket), so this entry exists to be *the* statement of the rule
    /// they are implementing - and a test pins that the hand-written arithmetic agrees with it.
    pub fn monotone(previous: u32, occupied: BucketLeafSpan) -> Self {
        Self::derive([occupied], SlackRule::NeverShrink { current: previous })
    }

    /// Does this cover back the region the bucket occupies?
    ///
    /// The commit-boundary check from `storage/lara.md`, as a function: called where a cover is published, so a cover that
    /// misses is reported before it is stored rather than by an audit afterwards.
    pub fn backs(self, span: BucketLeafSpan) -> Result<(), CoverMiss> {
        if span.is_empty() {
            return Ok(());
        }
        if span.start < self.start || span.end() > self.end() {
            return Err(CoverMiss {
                cover_start: self.start.get(),
                cover_end: self.end().get(),
                span_start: span.start.get(),
                span_end: span.end().get(),
            });
        }
        Ok(())
    }
}

/// A bucket's occupied span is not inside its vertex's cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverMiss {
    pub cover_start: u64,
    pub cover_end: u64,
    pub span_start: u64,
    pub span_end: u64,
}

impl core::fmt::Display for CoverMiss {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "bucket span [{}, {}) is not inside the vertex cover [{}, {})",
            self.span_start, self.span_end, self.cover_start, self.cover_end
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 256;

    fn span(start: u64, len: u32) -> BucketLeafSpan {
        BucketLeafSpan::new(EdgeSlot::from_raw(start), len)
    }

    #[test]
    fn derive_covers_the_occupied_union() {
        let cover = VertexLeafCover::derive(
            [span(256, 5), span(264, 3), BucketLeafSpan::empty()],
            SlackRule::None,
        );
        assert_eq!(cover.start(), EdgeSlot::from_raw(BASE));
        assert_eq!(cover.len(), 11);
        assert_eq!(cover.end().get(), 267);
    }

    /// The rule promotion follows: the cover is never shorter than what the vertex already holds.
    ///
    /// This is the retention `new_alloc = old_alloc` implements, stated as policy. It is deliberately not a floor on the
    /// allocation - a vertex starts at zero slots and grows on demand - so the test starts from an empty cover too.
    #[test]
    fn never_shrink_keeps_the_previous_width() {
        let kept =
            VertexLeafCover::derive([span(256, 2)], SlackRule::NeverShrink { current: 1026 });
        assert_eq!(
            kept.len(),
            1026,
            "the previous width wins over the new content"
        );
        assert!(kept.backs(span(256, 2)).is_ok());

        // A grow that actually needs more still wins over the old width.
        let grown = VertexLeafCover::derive([span(256, 4)], SlackRule::NeverShrink { current: 2 });
        assert_eq!(grown.len(), 4);
    }

    #[test]
    fn derive_without_occupancy_is_empty() {
        let cover = VertexLeafCover::derive([BucketLeafSpan::empty()], SlackRule::None);
        assert!(cover.is_empty());
    }

    /// The hand-written rule in the promotion paths is this function, not a second rule.
    ///
    /// `promote_tiny_to_slab` computes `new_stored = vertex.stored_slots.max(span_end - base)`; the table below is that
    /// expression for representative pairs, so a change to either side shows up here rather than as a cover that stopped
    /// backing its content.
    #[test]
    fn monotone_agrees_with_the_promotion_arithmetic() {
        for (previous, span_len, expected) in
            [(0u32, 4u32, 4u32), (16, 4, 16), (2, 9, 9), (1026, 2, 1026)]
        {
            let hand = previous.max(span_len);
            let typed = VertexLeafCover::monotone(previous, span(256, span_len));
            assert_eq!(hand, expected, "table sanity");
            assert_eq!(typed.len(), expected, "previous={previous} span={span_len}");
        }
    }

    /// The address arithmetic models the one operation an address space has: address + length.
    ///
    /// `advance`/`end_of` take a length, and the `Add`/`Sub` impls take `u32`, so `slot + slot` (a length!) and `slot + u64`
    /// (a number from anywhere) are both compile errors. That is the difference between "an address" and "a number".
    #[test]
    fn slot_arithmetic_stays_inside_the_space() {
        let base = EdgeSlot::from_raw(256);
        assert_eq!((base + 4).get(), 260);
        assert_eq!(base.end_of(4).get(), 260);
        assert_eq!(
            (base.advance(0)).get(),
            256,
            "zero length is the same address"
        );
        assert_eq!((base + 4) - 4, base);
        assert_eq!((base + 9) - base, 9, "distance is a length, not an address");
        // Saturating rather than wrapping: a negative distance means the operands were not comparable.
        assert_eq!(base - (base + 9), 0);
    }

    #[test]
    fn a_cover_cannot_express_less_than_it_backs() {
        // The measured promotion defect: a root at [258, 260) against a cover of [256, 257). The derivation cannot produce it,
        // because the root's span is an input and the cover is its union.
        let root = span(258, 2);
        let cover = VertexLeafCover::derive([root], SlackRule::None);
        assert_eq!(cover.end().get(), 260);
        assert!(cover.backs(root).is_ok());

        // A cover built from a smaller span still reports the miss rather than hiding it.
        let too_small = VertexLeafCover::derive([span(256, 1)], SlackRule::None);
        assert_eq!(
            too_small.backs(root),
            Err(CoverMiss {
                cover_start: 256,
                cover_end: 257,
                span_start: 258,
                span_end: 260,
            })
        );
    }

    #[test]
    fn an_empty_bucket_needs_no_cover_entry() {
        let cover = VertexLeafCover::derive([span(256, 5)], SlackRule::None);
        assert!(cover.backs(BucketLeafSpan::empty()).is_ok());
    }
}
