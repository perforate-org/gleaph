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
use super::compact::bucket_span_region_len;
use crate::labeled::record::LabelBucket;
use crate::lara::edge::EdgeStore;
use crate::traits::CsrEdgeTombstone;
use ic_stable_structures::Memory;

/// The region one bucket occupies inside the leaf, in the leaf's slot unit.
///
/// `empty()` covers the two cases where a bucket holds nothing in the leaf and therefore needs no cover entry: a tiny bucket
/// (its payload is inline) and a tree whose edge root was placed in an edge run (the root lives in the run, not in the leaf).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketLeafSpan {
    start: u64,
    len: u32,
}

impl BucketLeafSpan {
    /// The empty span: nothing owned in the leaf.
    pub const fn empty() -> Self {
        Self { start: 0, len: 0 }
    }

    fn new(start: u64, len: u32) -> Self {
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
                bucket.edge_start(),
                edges
                    .initialized_slab_slots(bucket)
                    .expect("initialized slab slots must be readable"),
            ),
            BucketMode::Tree if bucket.edge_run().is_some() => Self::empty(),
            BucketMode::Tree => Self::new(bucket.edge_start(), bucket_span_region_len(bucket)),
        }
    }

    /// Where the span starts, or zero when empty.
    pub fn start(self) -> u64 {
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
    pub fn end(self) -> u64 {
        self.start + u64::from(self.len)
    }
}

/// How a vertex's cover grows past what its buckets occupy.
///
/// The slack rule is data because it is a policy: the bucket-row slack (`5/4` growth, measured in
/// `GAP-2026-09-20-006` as worth its bytes) is the same idea one level down, and a cover that had no slack at all would
/// repack on every insert. It is a type rather than a constant so a caller states which rule it applied instead of leaving it
/// implicit in a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlackRule {
    /// Hold exactly what the buckets occupy.
    None,
    /// Grow to `ceil(len * num / den)` past the occupied end, as the bucket-row slack does.
    Grow { num: u32, den: u32 },
}

impl SlackRule {
    /// The rule the growth paths use today: `5/4`.
    pub const GROW_5_OF_4: Self = Self::Grow { num: 5, den: 4 };
}

/// A vertex's allocated region inside the leaf.
///
/// **Private fields, no length-taking constructor.** The only way to obtain one is [`Self::derive`], which starts from the
/// buckets' own spans, so a cover that does not back what the vertex occupies cannot be expressed - the promotion defect was
/// exactly that expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexLeafCover {
    start: u64,
    len: u32,
}

impl VertexLeafCover {
    /// The empty cover: a vertex that owns nothing in the leaf.
    pub const fn empty() -> Self {
        Self { start: 0, len: 0 }
    }

    /// The only constructor: the vertex's base address plus the spans its buckets occupy, grown by `slack`.
    ///
    /// Spans that lie before `base` or are empty are ignored, which is how the empty cases (`BucketLeafSpan::empty`) fall out
    /// without a second rule.
    pub fn derive(
        base: u64,
        spans: impl IntoIterator<Item = BucketLeafSpan>,
        slack: SlackRule,
    ) -> Self {
        let mut end = base;
        let mut any = false;
        for span in spans {
            if span.is_empty() || span.start < base {
                continue;
            }
            any = true;
            end = end.max(span.end());
        }
        if !any {
            return Self::empty();
        }
        let occupied = end - base;
        let grown = match slack {
            SlackRule::None => occupied,
            SlackRule::Grow { num, den } => {
                let den = u64::from(den).max(1);
                u64::from(u32::try_from(occupied).unwrap_or(u32::MAX))
                    .saturating_mul(u64::from(num))
                    .div_ceil(den)
                    .max(occupied)
            }
        };
        let len = u32::try_from(grown).expect("a leaf span cannot exceed u32 slots");
        Self { start: base, len }
    }

    pub fn start(self) -> u64 {
        self.start
    }

    pub fn len(self) -> u32 {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn end(self) -> u64 {
        self.start + u64::from(self.len)
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
                cover_start: self.start,
                cover_end: self.end(),
                span_start: span.start,
                span_end: span.end(),
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
        BucketLeafSpan::new(start, len)
    }

    #[test]
    fn derive_covers_the_occupied_union() {
        let cover = VertexLeafCover::derive(
            BASE,
            [span(256, 5), span(264, 3), BucketLeafSpan::empty()],
            SlackRule::None,
        );
        assert_eq!(cover.start(), BASE);
        assert_eq!(cover.len(), 11);
        assert_eq!(cover.end(), 267);
    }

    #[test]
    fn derive_applies_the_slack_rule_past_the_occupied_end() {
        let cover = VertexLeafCover::derive(BASE, [span(256, 4)], SlackRule::GROW_5_OF_4);
        assert_eq!(cover.len(), 5, "5/4 of four slots rounds up to five");
    }

    #[test]
    fn derive_without_occupancy_is_empty() {
        let cover =
            VertexLeafCover::derive(BASE, [BucketLeafSpan::empty()], SlackRule::GROW_5_OF_4);
        assert!(cover.is_empty());
    }

    #[test]
    fn a_cover_cannot_express_less_than_it_backs() {
        // The measured promotion defect: a root at [258, 260) against a cover of [256, 257). The derivation cannot produce it,
        // because the root's span is an input and the cover is its union.
        let root = span(258, 2);
        let cover = VertexLeafCover::derive(BASE, [root], SlackRule::None);
        assert_eq!(cover.end(), 260);
        assert!(cover.backs(root).is_ok());

        // A cover built from a smaller span still reports the miss rather than hiding it.
        let too_small = VertexLeafCover::derive(BASE, [span(256, 1)], SlackRule::None);
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
        let cover = VertexLeafCover::derive(BASE, [span(256, 5)], SlackRule::None);
        assert!(cover.backs(BucketLeafSpan::empty()).is_ok());
    }
}
