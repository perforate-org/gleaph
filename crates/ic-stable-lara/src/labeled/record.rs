//! Fixed-width records for the multi-level labeled CSR layout.

use crate::VertexId;
use crate::labeled::bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketLabelKey};
use crate::labeled::slot_index::{
    BUCKET_EDGE_RUN_LOW_BITS, BUCKET_TINY_MODE_BIT, OVERFLOW_LOG_NONE, checked_add_slot_index,
    clear_bucket_word_edge_run_low, decode_bucket_label_key, decode_bucket_word_edge_run_low,
    decode_meta28, decode_overflow_log_byte, decode_slot_index, encode_locator_word, read_u40,
    replace_bucket_label_key, replace_bucket_word_edge_run_low, slot_index_fits,
    try_encode_bucket_word, try_encode_locator_word, try_encode_overflow_log_byte,
    try_replace_slot_index, write_u40,
};
use crate::labeled::spill_run_store::{ArenaRows, SpillRunId};
use crate::slab_index::byte_offset_fits;
use crate::traits::{CsrEdge, CsrVertex, CsrVertexTombstone, NO_SPILL_RUN};
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

/// One LabelBucket descriptor in the intermediate CSR layer (24 bytes on wire).
///
/// Physical edge capacity within the containing [`LabeledVertex`] VertexEdgeSpan is
/// [`LabeledVertex::stored_slots`]; this row tracks one label's slab prefix and optional
/// overflow log.
///
/// [`Self::degree`] is the logical live edge count. Edge and inline-property-bytes physical
/// layouts are independent: [`Self::stored_slots`] counts edge slab slots, while
/// [`Self::inline_property_bytes_slab_slots`] counts inline-property-bytes slab slots. Each store has
/// its own overflow-log metadata and may fold or relocate without moving the other.
///
/// Per-mode field semantics (ADR 0096 §7.2 — every repurposed or constrained
/// field documents all three modes; a missing tiny meaning here is a review
/// rejection):
///
/// - `degree`: live count in ALL modes (tiny: 0..=3, see `TINY_MAX_DEGREE`).
/// - `stored_slots`: slab width for slab/tree; on tiny the live prefix width
///   (live + tombstones, ≤ 3) with `degree` = live (≤ stored) — wire rules,
///   `try_read_from` enforces.
/// - `edge_start` (word bits 0..36): span start for slab, root region for
///   tree, EMPTY-SPAN ANCHOR for tiny (valid successor boundary, §5).
/// - log-head (word bits 52..60): log head or NONE for slab/tree; on tiny
///   MUST be NONE (a zero decodes as a live log).
/// - `inline_property_bytes_slab_slots` (bytes 16..20): value-slab width for
///   slab/tree (always 0 at width 0); T0 inline target payload on tiny.
/// - `inline_property_bytes_offset` (bytes 20..24 + hi byte): value offset
///   for slab/tree; T1 (low-32) + T2-high-byte on tiny (hi-zero rule).
/// - `inline_property_byte_width` (bytes 25..26): value width for slab/tree;
///   T2 middle bytes on tiny (payload, never schema).
/// - `inline_property_bytes_log_byte` (byte 27): value-log head for
///   slab/tree; T2 top byte on tiny (payload, never schema).
/// - `inline_property_bytes_log_len` (byte 28): value-log length for
///   slab/tree; reserved zero on tiny (tail-zero rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBucket {
    word: u64,
    /// Logical live edge count for this label bucket.
    pub degree: u32,
    /// Stored edge-slab width (may exceed [`Self::degree`] while tombstones await
    /// compaction). Private since ADR 0096 §3b Phase 1 / plan 0361 R4: readers go
    /// through [`Self::stored_slots`] (mode-aware) or, where the wire meaning is
    /// owned, [`Self::stored_slots_raw`].
    stored_slots_raw: u32,
    /// Stored inline-property-bytes slab slots. Always zero when the inline property byte width is zero.
    inline_property_bytes_slab_slots: u32,
    /// Byte offset into [`EdgeInlinePropertyBytesStore`] where this bucket's value span starts.
    inline_property_bytes_offset: u64,
    /// Physical byte width per edge edge inline property slot (`0` = no values).
    inline_property_byte_width: u16,
    /// Wire byte for per-bucket inline property bytes overflow log head (`0xFF` = none).
    inline_property_bytes_log_byte: u8,
    /// Number of inline property bytes entries in this bucket's ordered suffix log.
    inline_property_bytes_log_len: u8,
    /// Edge-plane spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`]. The low
    /// [`BUCKET_EDGE_RUN_LOW_BITS`] live in `word`; the high 17 live in the bytes the row gained at
    /// 29..35, together with the values id and the named reserve.
    edge_spill_run: u32,
    /// Values-plane spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`]. 32 bits, in the added bytes.
    values_spill_run: u32,
}

impl LabelBucket {
    /// Mode-aware stored width (ADR 0096 §3b Phase 1 / plan 0361 R4).
    ///
    /// Slab and tree buckets return their stored edge-slab width (tombstones
    /// included). Tiny buckets return their live count: today the wire stores the
    /// live prefix width in this field position, and Phase 2 (K=4) repurposes
    /// those bytes for a fourth target — this accessor is the seam that keeps both
    /// meanings behind one name, which is why no reader outside this type touches
    /// the raw field. Sites that own the *wire* meaning (validation, tiny prefix
    /// iteration where ordinals include tombstone holes) call
    /// [`Self::stored_slots_raw`].
    pub(crate) fn stored_slots(&self) -> u32 {
        if self.is_tiny_mode() {
            self.degree
        } else {
            self.stored_slots_raw
        }
    }

    /// The raw field value, for validation and tiny prefix iteration only.
    pub(crate) fn stored_slots_raw(&self) -> u32 {
        self.stored_slots_raw
    }
}

impl Default for LabelBucket {
    fn default() -> Self {
        Self::from_parts(BucketLabelKey::default(), 0, 0, 0)
    }
}

impl LabelBucket {
    /// Fixed byte width of one encoded LabelBucket.
    ///
    /// ADR 0097 §3: 29 -> 36 bytes. The added 7 bytes hold the edge run id's high 17 bits, the whole
    /// values run id (32 bits) and a 7-bit named reserve (bits 49..56) - "reserve for the next bound
    /// widening, not padding". Those bits cannot raise the edge arena's bound: the shared unlabeled
    /// `Vertex` tail28 sets it (`design/storage/lara.md`). The final layout is 34 bytes, when slice 5
    /// deletes the values log and its two bytes 27-28 join the added region; that move is documented
    /// in ADR 0097 and has no code path yet.
    pub const BYTES: usize = 36;
    /// Edge-plane spill run id bits: [`BUCKET_EDGE_RUN_LOW_BITS`] in `word`, the rest in the added
    /// bytes.
    pub const EDGE_RUN_BITS: u32 = 27;
    /// High bits of the edge run id that live in the added bytes.
    pub const EDGE_RUN_HIGH_BITS: u32 = Self::EDGE_RUN_BITS - BUCKET_EDGE_RUN_LOW_BITS;
    /// Values-plane spill run id bits, all in the added bytes.
    pub const VALUES_RUN_BITS: u32 = 32;
    /// Named reserve in the added bytes (bits 49..56): reserve for the next bound widening, not
    /// padding, and it cannot raise the edge arena's bound (the shared tail28 sets it).
    pub const SPILL_ID_RESERVE_BITS: u32 = 7;
    /// The edge run field's all-ones pattern: not a legal id (`ArenaRows::EDGE.ids()` is
    /// `2^27 - 1`, so ids are `0..=2^27 - 2`), which is why it is the wire spelling of
    /// `NO_SPILL_RUN` and decodes back to it.
    pub const EDGE_RUN_ALL_ONES: u32 = (1 << Self::EDGE_RUN_BITS) - 1;

    /// Builds a row from logical fields.
    #[inline]
    pub fn from_parts(
        bucket_label_key: BucketLabelKey,
        edge_start: u64,
        degree: u32,
        stored_slots: u32,
    ) -> Self {
        Self::try_from_parts(
            bucket_label_key,
            edge_start,
            degree,
            stored_slots,
            0,
            0,
            0,
            -1,
            0,
        )
        .expect("LabelBucket::from_parts: invalid fields")
    }

    /// Builds a row with edge-inline-property-bytes fields.
    #[inline]
    pub fn from_parts_with_inline_property(
        bucket_label_key: BucketLabelKey,
        edge_start: u64,
        degree: u32,
        stored_slots: u32,
        inline_property_byte_width: u16,
        inline_property_bytes_offset: u64,
        inline_property_bytes_slab_slots: u32,
        inline_property_bytes_log_head: i32,
        inline_property_bytes_log_len: u8,
    ) -> Self {
        Self::try_from_parts(
            bucket_label_key,
            edge_start,
            degree,
            stored_slots,
            inline_property_byte_width,
            inline_property_bytes_offset,
            inline_property_bytes_slab_slots,
            inline_property_bytes_log_head,
            inline_property_bytes_log_len,
        )
        .expect("LabelBucket::from_parts_with_inline_property: invalid fields")
    }

    /// Fallible constructor with release-safe range checks.
    #[inline]
    pub fn try_from_parts(
        bucket_label_key: BucketLabelKey,
        edge_start: u64,
        degree: u32,
        stored_slots: u32,
        inline_property_byte_width: u16,
        inline_property_bytes_offset: u64,
        inline_property_bytes_slab_slots: u32,
        inline_property_bytes_log_head: i32,
        inline_property_bytes_log_len: u8,
    ) -> Result<Self, LabelBucketFieldError> {
        if !slot_index_fits(edge_start) {
            return Err(LabelBucketFieldError::SlotIndexOverflow);
        }
        if !byte_offset_fits(inline_property_bytes_offset) {
            return Err(LabelBucketFieldError::InlinePropertyBytesOffsetOverflow);
        }
        let word = try_encode_bucket_word(edge_start, bucket_label_key)
            .ok_or(LabelBucketFieldError::SlotIndexOverflow)?;
        let inline_property_bytes_log_byte =
            try_encode_overflow_log_byte(inline_property_bytes_log_head)
                .ok_or(LabelBucketFieldError::InlinePropertyBytesLogHeadOutOfRange)?;
        if inline_property_bytes_log_len > 170 {
            return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
        }
        if (inline_property_bytes_log_head < 0) != (inline_property_bytes_log_len == 0) {
            // **Plan 0318 §Step 7 exception**: tree-mode buckets may
            // have a nonzero `inline_property_bytes_log_len` while
            // `inline_property_bytes_log_byte` remains the "none"
            // encoding: the byte is repurposed as the packed depth
            // marker (edge depth - 1 in bits 0-1, property depth - 1
            // in bits 2-3, Plan 0327). The "mismatch" only fires for
            // non-tree buckets, where the existing rule still holds.
            let is_tree = (word & Self::TREE_MODE_BIT) != 0;
            if !is_tree {
                return Err(LabelBucketFieldError::InlinePropertyBytesLogStateMismatch);
            }
            if inline_property_bytes_log_len > 10 || (inline_property_bytes_log_len & 0b11) > 2 {
                return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
            }
        }
        if inline_property_byte_width == 0
            && (inline_property_bytes_slab_slots != 0 || inline_property_bytes_log_len != 0)
        {
            // **Plan 0318 §Step 7 exception**: for tree-mode buckets, the
            // `inline_property_bytes_log_len` byte is repurposed to
            // store the physical depth minus 1 (0, 1, or 2). Inline
            // properties are not allowed in tree mode (promote is
            // fail-closed on `inline_property_byte_width != 0`), so the
            // only consumer of this byte in tree mode is the depth
            // marker. For non-tree-mode buckets the existing rule
            // holds: `inline_property_bytes_log_len` must be 0 when
            // `inline_property_byte_width == 0`.
            let word = try_encode_bucket_word(edge_start, bucket_label_key)
                .ok_or(LabelBucketFieldError::SlotIndexOverflow)?;
            let is_tree = (word & Self::TREE_MODE_BIT) != 0;
            if !is_tree {
                return Err(LabelBucketFieldError::InlinePropertyBytesStateWithoutSchema);
            }
            if inline_property_bytes_log_len > 2 {
                return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
            }
        }
        Ok(Self {
            word,
            degree,
            stored_slots_raw: stored_slots,
            inline_property_bytes_slab_slots,
            inline_property_bytes_offset,
            inline_property_byte_width,
            inline_property_bytes_log_byte,
            inline_property_bytes_log_len,
            edge_spill_run: NO_SPILL_RUN,
            values_spill_run: NO_SPILL_RUN,
        })
    }

    /// Label key for this bucket row (directedness in the MSB).
    #[inline]
    pub fn bucket_label_key(self) -> BucketLabelKey {
        decode_bucket_label_key(self.word)
    }

    /// Bit 63 of the packed `word`: 1 = tree mode (LTB-backed), 0 = slab mode.
    ///
    /// Plan 0318 §Step 1 / ADR 0088 §1. Slab-mode buckets (the historical
    /// default) have bit 63 = 0 and reopen unchanged.
    pub(crate) const TREE_MODE_BIT: u64 = crate::labeled::slot_index::BUCKET_TREE_MODE_BIT;

    /// Returns `true` if this bucket is in tree mode (LTB-backed).
    ///
    /// The check is a single bit-test on the packed `word`. The wire format
    /// (29 bytes) is unchanged: bit 63 lives in `bytes[7]`, the high byte of
    /// the existing 8-byte `word` prefix.
    #[inline]
    pub fn is_tree_mode(&self) -> bool {
        (self.word & Self::TREE_MODE_BIT) != 0
    }

    /// Returns a copy of this bucket with the tree-mode flag set or cleared.
    ///
    /// The other fields (`bucket_label_key`, `edge_start`, `degree`, `stored_slots`,
    /// `inline_property_bytes_*`) are preserved.
    #[inline]
    pub fn with_tree_mode(mut self, enabled: bool) -> Self {
        if enabled {
            self.word |= Self::TREE_MODE_BIT;
        } else {
            self.word &= !Self::TREE_MODE_BIT;
        }
        self
    }

    /// Maximum live edges for a tiny-mode bucket (ADR 0096 §3 / §3b Phase 2).
    ///
    /// Wire truth (validated at `try_read_from` and `try_enable_tiny_mode`), not
    /// policy. K=4 uses the former `stored_slots` position (bytes 12..16) as the
    /// fourth target and byte 28 (reserved zero for K≤3) as the **used slot
    /// width** (0..=4): live targets may be 0 and holes are tombstones, so the
    /// used width cannot be derived — it bounds hole scans, appends
    /// (Insertion's tail ordinal), and promotion transcription.
    pub(crate) const TINY_MAX_DEGREE: u32 = 4;

    /// Bit 60 of the packed `word`: 1 = tiny mode (descriptor-resident inline
    /// targets), 0 = slab/tree interpretation. ADR 0096 §1.
    pub(crate) const TINY_MODE_BIT: u64 = BUCKET_TINY_MODE_BIT;

    /// Returns `true` if this bucket is in tiny mode (inline targets, zero slab
    /// slots, no overflow log, no inline-property schema). ADR 0096 §1.
    ///
    /// Single bit-test on the packed `word`; the 29-byte wire format is unchanged.
    #[inline]
    pub fn is_tiny_mode(&self) -> bool {
        (self.word & Self::TINY_MODE_BIT) != 0
    }

    /// Inline edge target at position `index` (0..3) for a tiny-mode bucket.
    ///
    /// Payload map (ADR 0096 §1): T0 ≡ `inline_property_bytes_slab_slots` value,
    /// T1 ≡ `inline_property_bytes_offset` low-32 (+hi-zero rule), T2 ≡ raw
    /// composition of offset-hi byte, `inline_property_byte_width`, and
    /// `inline_property_bytes_log_byte`. Meaningful only for tiny buckets:
    /// callers dispatch on [`Self::is_tiny_mode`] first (the bytes are inline
    /// property state on slab buckets). Debug-asserts tiny mode; the G6
    /// mode-matrix tests cover behavioral misuse.
    #[inline]
    pub fn tiny_target(self, index: u32) -> u32 {
        debug_assert!(
            self.is_tiny_mode(),
            "tiny_target requires a tiny-mode bucket"
        );
        debug_assert!(
            index < Self::TINY_MAX_DEGREE,
            "tiny target index out of range"
        );
        match index {
            0 => self.inline_property_bytes_slab_slots,
            1 => (self.inline_property_bytes_offset & 0xFFFF_FFFF) as u32,
            2 => {
                let hi = ((self.inline_property_bytes_offset >> 32) & 0xFF) as u32;
                hi | ((u32::from(self.inline_property_byte_width)) << 8)
                    | ((u32::from(self.inline_property_bytes_log_byte)) << 24)
            }
            3 => self.stored_slots_raw,
            _ => panic!("LabelBucket::tiny_target: index {index} exceeds TINY_MAX_DEGREE"),
        }
    }

    /// Used slot width of a tiny bucket (0..=TINY_MAX_DEGREE): the number of
    /// slots that are live or a tombstone hole. Slots outside it are unused and
    /// never read. Meaningful only for tiny buckets.
    ///
    /// Stored in byte 28 (reserved zero for K≤3); the field position is the
    /// inline-property log length for slab/tree, so callers must dispatch on
    /// [`Self::is_tiny_mode`] first.
    pub(crate) fn tiny_used_width(&self) -> u32 {
        debug_assert!(
            self.is_tiny_mode(),
            "tiny_used_width requires a tiny bucket"
        );
        u32::from(self.inline_property_bytes_log_len)
    }

    /// Returns a copy with the tiny used slot width set (`<= TINY_MAX_DEGREE`).
    #[inline]
    pub(crate) fn with_tiny_used_width(self, used: u32) -> Self {
        debug_assert!(
            self.is_tiny_mode(),
            "with_tiny_used_width requires a tiny bucket"
        );
        debug_assert!(
            used <= Self::TINY_MAX_DEGREE,
            "tiny used width exceeds TINY_MAX_DEGREE"
        );
        Self {
            inline_property_bytes_log_len: used as u8,
            ..self
        }
    }

    /// Returns a copy with inline tiny target `index` (0..4) set to `target`.
    ///
    /// Panics on `index >= TINY_MAX_DEGREE` (programmer error, mirroring the
    /// `from_parts` convention). Does not maintain the used width or validate
    /// tiny invariants — callers publish through paths validated by
    /// `try_read_from` / `try_enable_tiny_mode`.
    #[inline]
    pub fn with_tiny_target(self, index: u32, target: u32) -> Self {
        assert!(
            index < Self::TINY_MAX_DEGREE,
            "LabelBucket::with_tiny_target: index out of range"
        );
        debug_assert!(
            self.is_tiny_mode(),
            "with_tiny_target requires a tiny-mode bucket"
        );
        match index {
            0 => Self {
                inline_property_bytes_slab_slots: target,
                ..self
            },
            1 => Self {
                inline_property_bytes_offset: (self.inline_property_bytes_offset & !0xFFFF_FFFF)
                    | u64::from(target),
                ..self
            },
            2 => Self {
                inline_property_bytes_offset: (self.inline_property_bytes_offset & 0xFFFF_FFFF)
                    | ((u64::from(target & 0xFF)) << 32),
                inline_property_byte_width: ((target >> 8) & 0xFFFF) as u16,
                inline_property_bytes_log_byte: (target >> 24) as u8,
                ..self
            },
            3 => Self {
                stored_slots_raw: target,
                ..self
            },
            _ => panic!("LabelBucket::with_tiny_target: index {index} exceeds TINY_MAX_DEGREE"),
        }
    }

    /// Enables tiny mode on a compatible bucket, validating the checkable subset.
    ///
    /// Pre-checks (slab semantics, all checkable): tree bit clear, `degree ≤
    /// TINY_MAX_DEGREE`, word log-head NONE, and no live inline-property value
    /// state (width/slots/offset/log all empty — targets and the used width are
    /// set after enabling). The no-log sentinel is normalized into the zero byte
    /// the tiny wire rules require; a live value log is rejected, never
    /// destroyed. R2b dispatch additionally guarantees width-0 callers.
    /// Already-tiny input revalidates idempotently.
    pub fn try_enable_tiny_mode(self) -> Result<Self, LabelBucketFieldError> {
        if self.is_tiny_mode() {
            self.check_tiny_invariants()?;
            return Ok(self);
        }
        if self.is_tree_mode() {
            return Err(LabelBucketFieldError::TinyTreeModeConflict);
        }
        if self.degree > Self::TINY_MAX_DEGREE {
            return Err(LabelBucketFieldError::TinyDegreeOutOfRange);
        }
        // The former stored field becomes T3 payload and byte 28 becomes the used
        // width, so neither carries a pre-enable constraint beyond the value-state
        // checks below (which reject any live edge/property bytes).
        let mut out = self;
        if out.inline_property_bytes_log_byte == OVERFLOW_LOG_NONE
            && out.inline_property_bytes_log_len == 0
        {
            out.inline_property_bytes_log_byte = 0;
        }
        if out.inline_property_byte_width != 0
            || out.inline_property_bytes_slab_slots != 0
            || out.inline_property_bytes_offset != 0
            || out.inline_property_bytes_log_byte != 0
            || out.inline_property_bytes_log_len != 0
        {
            return Err(LabelBucketFieldError::TinyValueStatePresent);
        }
        out.word |= Self::TINY_MODE_BIT;
        // Slab prefix semantics: the live slots are 0..degree with no holes, so
        // the used width starts equal to `degree` (K=4 byte-28 rule) and the
        // slots beyond it are zeroed — a deterministic birth shape (they carry no
        // meaning and are never read, but a stale corpus value in T3 would
        // otherwise leak into the golden wire).
        out = out.with_tiny_used_width(out.degree);
        for index in out.degree..Self::TINY_MAX_DEGREE {
            out = out.with_tiny_target(index, 0);
        }
        out.check_tiny_invariants()?;
        Ok(out)
    }

    /// Validates the checkable tiny-mode invariants; requires the tiny bit set.
    /// Shared by `try_read_from` (wire) and `try_enable_tiny_mode` (memory).
    fn check_tiny_invariants(&self) -> Result<(), LabelBucketFieldError> {
        debug_assert!(
            self.is_tiny_mode(),
            "check_tiny_invariants requires the tiny bit"
        );
        if self.is_tree_mode() {
            return Err(LabelBucketFieldError::TinyTreeModeConflict);
        }
        if self.degree > Self::TINY_MAX_DEGREE {
            return Err(LabelBucketFieldError::TinyDegreeOutOfRange);
        }
        // K=4 tiny wire rules: `degree` = live count (≤ 4); T0..T3 are free
        // payload (T3 sits in the former stored-slot position); byte 28 carries
        // the used slot width (size ≤ the cap, and at least `degree` because every
        // live slot is inside the used range). Dead-slot content is
        // layout-native (`E::tombstone_edge()` encoded, read back via
        // `E::is_deleted_slot()`), so validation stays range-only (E-agnostic).
        let used = u32::from(self.inline_property_bytes_log_len);
        if used > Self::TINY_MAX_DEGREE || used < self.degree {
            return Err(LabelBucketFieldError::TinyUsedWidthOutOfRange);
        }
        // ADR 0097 §3: the level-1 spill arena serves slab buckets only.
        if self.edge_spill_run != NO_SPILL_RUN || self.values_spill_run != NO_SPILL_RUN {
            return Err(LabelBucketFieldError::SpillRunOnNonSlabBucket);
        }
        Ok(())
    }

    /// Plan 0318 §Step 7: physical depth of the tree-mode layout, in
    /// the range `1..=MAX_DEPTH = 3`. For a freshly-promoted bucket
    /// (no `tree_mode_deepen` call) the physical depth equals
    /// `tree_csr::derive_depth(stored_slots)`. After a
    /// `tree_mode_deepen` call the physical depth is one more than the
    /// pre-call depth (capped at `MAX_DEPTH`); the structural
    /// formula's view (`derive_depth(stored_slots)`) is unchanged, so
    /// the resolver needs an explicit depth marker to disambiguate.
    ///
    /// The marker is stored in the `inline_property_bytes_log_len`
    /// byte (which is required to be 0 for tree-mode buckets with no
    /// inline property bytes) as `depth - 1`. The byte range used is
    /// `0..=2`; the validation in `try_from_parts` and `try_read_from`
    /// enforces this for tree-mode buckets and rejects larger values.
    ///
    /// For non-tree-mode buckets the marker is unused and the
    /// `inline_property_bytes_log_len` byte keeps its existing
    /// semantic (number of entries in the inline property bytes
    /// overflow log, range `0..=170`).
    ///
    /// Plan 0327: the same byte also carries the property-tree
    /// physical depth in bits 2-3 (see
    /// [`Self::tree_mode_property_depth`]). All access goes through
    /// these accessors — never touch `inline_property_bytes_log_len`
    /// directly for tree-mode buckets.
    #[inline]
    pub fn tree_mode_physical_depth(&self) -> u32 {
        if !self.is_tree_mode() {
            // Non-tree buckets: physical depth is irrelevant; return 0
            // so callers can check `is_tree_mode() == false` to skip
            // depth-aware logic.
            return 0;
        }
        // Tree buckets: edge depth = (low 2 bits) + 1, capped at MAX_DEPTH.
        (u32::from(self.inline_property_bytes_log_len) & 0b11) + 1
    }

    /// Plan 0327: physical depth of the property tree (LPB-in-tree)
    /// in a tree-mode bucket, in the range `1..=MAX_PROPERTY_DEPTH = 3`.
    /// Depth 1 means the property root holds LPB leaf block_ids
    /// directly; depth `d >= 2` means the root holds
    /// `InlinePropertyInterior` block_ids one hop above the leaves.
    ///
    /// Stored in bits 2-3 of the repurposed
    /// `inline_property_bytes_log_len` byte (packed next to the
    /// edge-tree depth marker). For `w == 0` buckets the property
    /// depth is always 1 (validation enforces zero bits).
    #[inline]
    pub fn tree_mode_property_depth(&self) -> u32 {
        ((u32::from(self.inline_property_bytes_log_len) >> 2) & 0b11) + 1
    }

    /// Returns a copy with the tree-mode edge physical depth set.
    /// `depth` must be in `1..=MAX_DEPTH = 3`. Preserves the packed
    /// property-depth bits. See [`Self::tree_mode_physical_depth`] for
    /// the encoding.
    #[inline]
    pub fn with_tree_mode_physical_depth(mut self, depth: u32) -> Self {
        debug_assert!(
            (1..=3).contains(&depth),
            "tree_mode_physical_depth out of range"
        );
        self.inline_property_bytes_log_len =
            (self.inline_property_bytes_log_len & !0b11) | (depth - 1) as u8;
        self
    }

    /// Returns a copy with the tree-mode property-tree physical depth
    /// set. `depth` must be in `1..=3`. Preserves the packed
    /// edge-depth bits. See [`Self::tree_mode_property_depth`] for the
    /// encoding.
    #[inline]
    pub fn with_tree_mode_property_depth(mut self, depth: u32) -> Self {
        debug_assert!(
            (1..=3).contains(&depth),
            "tree_mode_property_depth out of range"
        );
        self.inline_property_bytes_log_len =
            (self.inline_property_bytes_log_len & !0b1100) | ((depth - 1) as u8) << 2;
        self
    }

    /// Global edge-slot index where this bucket's slab prefix starts.
    #[inline]
    pub fn edge_start(self) -> u64 {
        decode_slot_index(self.word)
    }

    /// Edge-plane spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`] when nothing has spilled.
    #[inline]
    pub fn edge_spill_run(self) -> u32 {
        self.edge_spill_run
    }

    /// Fallible setter for the edge-plane spill run id: it must fit the shared arena's capacity
    /// ([`ArenaRows::EDGE`], the unlabeled tail28's 27-bit range) and must not be the sentinel.
    #[inline]
    pub fn try_with_edge_spill_run(self, run: u32) -> Result<Self, LabelBucketFieldError> {
        if SpillRunId::mint(run, ArenaRows::EDGE).is_err() {
            return Err(LabelBucketFieldError::SpillRunOutOfRange);
        }
        Ok(Self {
            edge_spill_run: run,
            ..self
        })
    }

    /// Returns a copy with the edge-plane spill run id set.
    #[inline]
    pub fn with_edge_spill_run(self, run: u32) -> Self {
        self.try_with_edge_spill_run(run)
            .expect("LabelBucket::with_edge_spill_run: run id out of range")
    }

    /// Returns a copy with no edge-plane spill run: every edge row lives in the bucket's slab
    /// prefix (ADR 0097 §4).
    ///
    /// Clearing is its own infallible transition: [`Self::try_with_edge_spill_run`] validates
    /// *live* run ids and so rejects the sentinel, while a released (or never-allocated) run must
    /// still be representable.
    #[inline]
    pub fn without_edge_spill_run(self) -> Self {
        Self {
            edge_spill_run: NO_SPILL_RUN,
            ..self
        }
    }

    /// Values-plane spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`] when nothing has spilled.
    #[inline]
    pub fn values_spill_run(self) -> u32 {
        self.values_spill_run
    }

    /// Fallible setter for the values-plane spill run id: it must fit [`ArenaRows::VALUES`] and must
    /// not be the sentinel.
    #[inline]
    pub fn try_with_values_spill_run(self, run: u32) -> Result<Self, LabelBucketFieldError> {
        if SpillRunId::mint(run, ArenaRows::VALUES).is_err() {
            return Err(LabelBucketFieldError::SpillRunOutOfRange);
        }
        Ok(Self {
            values_spill_run: run,
            ..self
        })
    }

    /// Returns a copy with the values-plane spill run id cleared. The sentinel cannot go through
    /// [`Self::try_with_values_spill_run`], which exists to reject it, so clearing is its own
    /// operation (mirroring [`Self::without_edge_spill_run`]).
    #[inline]
    pub fn without_values_spill_run(self) -> Self {
        Self {
            values_spill_run: NO_SPILL_RUN,
            ..self
        }
    }

    /// Returns a copy with the values-plane spill run id set.
    #[inline]
    pub fn with_values_spill_run(self, run: u32) -> Self {
        self.try_with_values_spill_run(run)
            .expect("LabelBucket::with_values_spill_run: run id out of range")
    }

    /// Byte offset into `EdgeInlinePropertyBytesStore` for this bucket's value span.
    #[inline]
    pub fn inline_property_bytes_offset(self) -> u64 {
        self.inline_property_bytes_offset
    }

    /// Number of inline-property-bytes entries resident in the inline property bytes slab.
    #[inline]
    pub fn inline_property_bytes_slab_slots(self) -> u32 {
        self.inline_property_bytes_slab_slots
    }

    /// Per-bucket inline property bytes overflow log head, or `-1` when all values are on the slab.
    #[inline]
    pub fn inline_property_bytes_log_head(self) -> i32 {
        decode_overflow_log_byte(self.inline_property_bytes_log_byte)
    }

    /// Number of values in the ordered inline-property-bytes-log suffix.
    #[inline]
    pub fn inline_property_bytes_log_len(self) -> u8 {
        self.inline_property_bytes_log_len
    }

    /// Physical byte width per edge edge inline property slot (`0` = no values).
    #[inline]
    pub fn inline_property_byte_width(self) -> u16 {
        self.inline_property_byte_width
    }

    /// Returns `true` when this bucket owns a non-empty value span.
    #[inline]
    pub fn is_inline_property_bytes_allocated(self) -> bool {
        // ADR 0096 §5 value funnel: tiny buckets hold no value state by
        // construction, so they report unallocated here. This single arm covers
        // every `!allocated || width == 0` early-return path (values reads,
        // writes, compaction, residents) without further arms; direct width
        // readers outside those paths are diverted at §5 entry points instead.
        if self.is_tiny_mode() {
            return false;
        }
        self.inline_property_byte_width != 0 && self.degree != 0
    }

    #[inline]
    fn with_word(mut self, word: u64) -> Self {
        self.word = word;
        self
    }

    /// Returns a copy with [`Self::inline_property_byte_width`] updated.
    #[inline]
    pub fn with_inline_property_byte_width(self, inline_property_byte_width: u16) -> Self {
        Self {
            inline_property_byte_width,
            ..self
        }
    }

    /// Returns a copy with [`Self::inline_property_bytes_offset`] updated.
    #[inline]
    pub fn with_inline_property_bytes_offset(self, inline_property_bytes_offset: u64) -> Self {
        Self {
            inline_property_bytes_offset,
            ..self
        }
    }

    /// Returns a copy with the inline property bytes slab slot count updated.
    #[inline]
    pub fn with_inline_property_bytes_slab_slots(
        self,
        inline_property_bytes_slab_slots: u32,
    ) -> Self {
        Self {
            inline_property_bytes_slab_slots,
            ..self
        }
    }

    /// Returns a copy with [`Self::inline_property_bytes_log_head`] updated.
    #[inline]
    pub fn try_with_inline_property_bytes_log_head(
        self,
        head: i32,
    ) -> Result<Self, LabelBucketFieldError> {
        let inline_property_bytes_log_byte = try_encode_overflow_log_byte(head)
            .ok_or(LabelBucketFieldError::InlinePropertyBytesLogHeadOutOfRange)?;
        let inline_property_bytes_log_len = if head < 0 {
            0
        } else {
            self.inline_property_bytes_log_len.max(1)
        };
        Ok(Self {
            inline_property_bytes_log_byte,
            inline_property_bytes_log_len,
            ..self
        })
    }

    /// Returns a copy with [`Self::inline_property_bytes_log_head`] and [`Self::inline_property_bytes_log_len`] updated.
    #[inline]
    pub fn try_with_inline_property_bytes_log(
        self,
        head: i32,
        len: u8,
    ) -> Result<Self, LabelBucketFieldError> {
        let inline_property_bytes_log_byte = try_encode_overflow_log_byte(head)
            .ok_or(LabelBucketFieldError::InlinePropertyBytesLogHeadOutOfRange)?;
        if len > 170 {
            return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
        }
        if (head < 0) != (len == 0) {
            return Err(LabelBucketFieldError::InlinePropertyBytesLogStateMismatch);
        }
        Ok(Self {
            inline_property_bytes_log_byte,
            inline_property_bytes_log_len: len,
            ..self
        })
    }

    /// Returns a copy with [`Self::inline_property_bytes_log_head`] updated.
    #[inline]
    pub fn with_inline_property_bytes_log_head(self, head: i32) -> Self {
        self.try_with_inline_property_bytes_log_head(head)
            .expect("LabelBucket::with_inline_property_bytes_log_head: head out of range")
    }

    /// Encodes this LabelBucket into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        let LabelBucket {
            word,
            degree,
            stored_slots_raw: stored_slots,
            inline_property_bytes_slab_slots,
            inline_property_bytes_offset,
            inline_property_byte_width,
            inline_property_bytes_log_byte,
            inline_property_bytes_log_len,
            edge_spill_run,
            values_spill_run,
        } = self;
        let word = replace_bucket_word_edge_run_low(
            word,
            edge_spill_run & ((1 << BUCKET_EDGE_RUN_LOW_BITS) - 1),
        );
        bytes[0..8].copy_from_slice(&word.to_le_bytes());
        bytes[8..12].copy_from_slice(&degree.to_le_bytes());
        bytes[12..16].copy_from_slice(&stored_slots.to_le_bytes());
        bytes[16..20].copy_from_slice(&inline_property_bytes_slab_slots.to_le_bytes());
        let value_wire: &mut [u8; 5] = (&mut bytes[20..25])
            .try_into()
            .expect("LabelBucket inline_property_bytes_offset wire slice must be 5 bytes");
        write_u40(inline_property_bytes_offset, value_wire);
        bytes[25..27].copy_from_slice(&inline_property_byte_width.to_le_bytes());
        bytes[27] = inline_property_bytes_log_byte;
        bytes[28] = inline_property_bytes_log_len;
        // The added region: edge high 17 | values 32 | reserve 7 (56 bits).
        // The edge run's high bits must be masked to their field width before they share the
        // packed word with the values id: the sentinel (`u32::MAX`) shifts down to more bits than
        // the field holds, and without the mask those extra bits corrupt the values run id.
        let packed = (u64::from(edge_spill_run >> BUCKET_EDGE_RUN_LOW_BITS)
            & ((1u64 << Self::EDGE_RUN_HIGH_BITS) - 1))
            | (u64::from(values_spill_run) << Self::EDGE_RUN_HIGH_BITS);
        bytes[29..36].copy_from_slice(&packed.to_le_bytes()[..7]);
    }

    /// Returns a copy with `edge_start` / [`Self::stored_slots`] updated.
    #[inline]
    pub fn with_edge_range(self, edge_start: u64, stored_slots: u32) -> Self {
        self.try_with_edge_range(edge_start, stored_slots)
            .expect("LabelBucket::with_edge_range: edge_start out of 36-bit range")
    }

    /// Fallible [`Self::with_edge_range`].
    #[inline]
    pub fn try_with_edge_range(
        self,
        edge_start: u64,
        stored_slots: u32,
    ) -> Result<Self, LabelBucketFieldError> {
        let word = try_replace_slot_index(self.word, edge_start)
            .ok_or(LabelBucketFieldError::SlotIndexOverflow)?;
        Ok(Self {
            word,
            stored_slots_raw: stored_slots,
            ..self
        })
    }

    /// Returns a copy with [`Self::bucket_label_key`] updated.
    #[inline]
    pub fn with_bucket_label_key(self, bucket_label_key: BucketLabelKey) -> Self {
        self.with_word(replace_bucket_label_key(self.word, bucket_label_key))
    }

    /// Returns a copy with [`Self::degree`] updated.
    #[inline]
    pub fn with_degree_field(self, degree: u32) -> Self {
        Self { degree, ..self }
    }

    /// Returns a copy with [`Self::stored_slots`] updated.
    ///
    /// Slab/tree only: a tiny bucket encodes neither a slab extent nor a live
    /// count in this field (T3 is payload, see [`Self::tiny_used_width`]), so
    /// writing it there would corrupt an inline target. Use
    /// [`Self::with_tiny_used_width`] for tiny buckets.
    #[inline]
    pub fn with_stored_slots(self, stored_slots: u32) -> Self {
        debug_assert!(
            !self.is_tiny_mode(),
            "with_stored_slots requires a non-tiny bucket"
        );
        Self {
            stored_slots_raw: stored_slots,
            ..self
        }
    }

    /// Decodes a LabelBucket from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        Self::try_read_from(bytes).expect("invalid LabelBucket wire bytes")
    }

    /// Decodes and validates a LabelBucket from exactly [`Self::BYTES`] bytes.
    pub fn try_read_from(bytes: &[u8]) -> Result<Self, LabelBucketFieldError> {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabelBucket::try_read_from expects exactly Self::BYTES bytes");
        let word = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let inline_property_bytes_slab_slots =
            u32::from_le_bytes(chunk[16..20].try_into().unwrap());
        let inline_property_bytes_offset = read_u40(&chunk[20..25].try_into().unwrap());
        if !byte_offset_fits(inline_property_bytes_offset) {
            return Err(LabelBucketFieldError::InlinePropertyBytesOffsetOverflow);
        }
        let inline_property_byte_width = u16::from_le_bytes(chunk[25..27].try_into().unwrap());
        let inline_property_bytes_log_byte = chunk[27];
        let inline_property_bytes_log_len = chunk[28];
        let mut added = [0u8; 8];
        added[..7].copy_from_slice(&chunk[29..36]);
        let added = u64::from_le_bytes(added);
        let edge_run_bits = decode_bucket_word_edge_run_low(word)
            | (((added & ((1u64 << Self::EDGE_RUN_HIGH_BITS) - 1)) as u32)
                << BUCKET_EDGE_RUN_LOW_BITS);
        // The wire word carries the run id's low bits; the decoded row keeps the id in its own
        // field only, so the word's copy is cleared (`write_to` re-injects it).
        let word = clear_bucket_word_edge_run_low(word);
        // ADR 0097 §3: the all-ones pattern is not a legal id, so it is the sentinel's wire
        // spelling and must decode back to it. Without this mapping a row written with
        // `NO_SPILL_RUN` would read back as a live-looking id (2^27 - 1) and every reader that
        // asks "does this row own a run?" would answer yes.
        let edge_spill_run = if edge_run_bits == Self::EDGE_RUN_ALL_ONES {
            NO_SPILL_RUN
        } else {
            edge_run_bits
        };
        let values_spill_run =
            ((added >> Self::EDGE_RUN_HIGH_BITS) & ((1u64 << Self::VALUES_RUN_BITS) - 1)) as u32;
        // ADR 0096 §1: tiny buckets carry targets (not value/log state) in the
        // inline-property bytes. Validate the tiny wire rules here and return
        // before the slab/tree consistency rules below, which would misread
        // payload as schema (e.g. a T2 top byte in 170..254 is not a log head,
        // and live targets are not value slots). Wire truth: TINY_MAX_DEGREE,
        // NONE log-head, stored == degree, zero tail (see check_tiny_invariants).
        let bucket = Self {
            word,
            degree: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            stored_slots_raw: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            inline_property_bytes_slab_slots,
            inline_property_bytes_offset,
            inline_property_byte_width,
            inline_property_bytes_log_byte,
            inline_property_bytes_log_len,
            edge_spill_run,
            values_spill_run,
        };
        if bucket.is_tiny_mode() {
            bucket.check_tiny_invariants()?;
            return Ok(bucket);
        }
        if inline_property_bytes_log_byte != OVERFLOW_LOG_NONE
            && inline_property_bytes_log_byte >= 170
        {
            return Err(LabelBucketFieldError::InlinePropertyBytesLogHeadOutOfRange);
        }
        if inline_property_bytes_log_len > 170 {
            return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
        }
        if (inline_property_bytes_log_byte == OVERFLOW_LOG_NONE)
            != (inline_property_bytes_log_len == 0)
        {
            // **Plan 0318 §Step 7 exception**: tree-mode buckets may
            // have a nonzero `inline_property_bytes_log_len` while
            // `inline_property_bytes_log_byte` remains `OVERFLOW_LOG_NONE`:
            // the byte is repurposed as the packed depth marker
            // (edge depth - 1 in bits 0-1, property depth - 1 in
            // bits 2-3, Plan 0327). The "mismatch" only fires for
            // non-tree buckets, where the existing rule still holds.
            let is_tree = (word & Self::TREE_MODE_BIT) != 0;
            if !is_tree {
                return Err(LabelBucketFieldError::InlinePropertyBytesLogStateMismatch);
            }
            if inline_property_bytes_log_len > 10 || (inline_property_bytes_log_len & 0b11) > 2 {
                return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
            }
        }
        if inline_property_byte_width == 0
            && (inline_property_bytes_slab_slots != 0 || inline_property_bytes_log_len != 0)
        {
            // **Plan 0318 §Step 7 exception**: for tree-mode buckets,
            // the `inline_property_bytes_log_len` byte is repurposed to
            // store the physical depth minus 1 (0, 1, or 2). Inline
            // properties are not allowed in tree mode (promote is
            // fail-closed on `inline_property_byte_width != 0`), so the
            // only consumer of this byte in tree mode is the depth
            // marker. For non-tree-mode buckets the existing rule
            // holds: `inline_property_bytes_log_len` must be 0 when
            // `inline_property_byte_width == 0`.
            let is_tree = (word & Self::TREE_MODE_BIT) != 0;
            if !is_tree {
                return Err(LabelBucketFieldError::InlinePropertyBytesStateWithoutSchema);
            }
            if inline_property_bytes_log_len > 2 {
                return Err(LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange);
            }
        }
        // ADR 0097 §3: tree buckets are already chunked storage, so the arena never serves them.
        if (word & Self::TREE_MODE_BIT) != 0
            && (edge_spill_run != NO_SPILL_RUN || values_spill_run != NO_SPILL_RUN)
        {
            return Err(LabelBucketFieldError::SpillRunOnNonSlabBucket);
        }
        Ok(bucket)
    }
}

/// Invalid [`LabelBucket`] wire or field combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelBucketFieldError {
    /// Bits 61–62 of the packed word are reserved and must be zero (bit 60 is
    /// `edge_start` does not fit in the 36-bit slot index.
    SlotIndexOverflow,
    /// `inline_property_bytes_offset` does not fit in the 40-bit byte-offset space.
    InlinePropertyBytesOffsetOverflow,
    /// Value overflow log head byte is out of range.
    InlinePropertyBytesLogHeadOutOfRange,
    /// Value overflow log length byte is out of range.
    InlinePropertyBytesLogLenOutOfRange,
    /// Value overflow log head and length disagree.
    InlinePropertyBytesLogStateMismatch,
    /// InlinePropertyBytes slots/log entries require a non-zero inline property schema width.
    InlinePropertyBytesStateWithoutSchema,
    /// Tiny-mode bit set together with the tree-mode bit (ADR 0096 §1).
    TinyTreeModeConflict,
    /// Tiny-mode bucket with `degree > TINY_MAX_DEGREE`.
    TinyDegreeOutOfRange,
    /// Tiny-mode bucket whose byte-28 used width exceeds `TINY_MAX_DEGREE` or is
    /// smaller than `degree` (every live slot lies inside the used range). K=4.
    TinyUsedWidthOutOfRange,

    /// Enabling tiny mode on a bucket with live inline-property value state
    /// (width, slots, offset, or log entries present). ADR 0096 §1.
    TinyValueStatePresent,
    /// A run id is out of the arena's range, is the `u32::MAX` sentinel, or is present on a bucket
    /// whose mode has no level-1 spill (tiny or tree). ADR 0097 §3.
    SpillRunOutOfRange,
    /// A run id is present on a bucket whose mode has no level-1 spill (tiny or tree).
    SpillRunOnNonSlabBucket,
}

impl core::fmt::Display for LabelBucketFieldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SlotIndexOverflow => {
                write!(f, "label bucket edge_start exceeds 36-bit slot index")
            }
            Self::InlinePropertyBytesOffsetOverflow => {
                write!(
                    f,
                    "label bucket inline_property_bytes_offset exceeds 40-bit byte offset"
                )
            }
            Self::InlinePropertyBytesLogHeadOutOfRange => {
                write!(
                    f,
                    "label bucket inline property bytes log head out of range"
                )
            }
            Self::InlinePropertyBytesLogLenOutOfRange => {
                write!(
                    f,
                    "label bucket inline property bytes log length out of range"
                )
            }
            Self::InlinePropertyBytesLogStateMismatch => {
                write!(
                    f,
                    "label bucket inline property bytes log head/length mismatch"
                )
            }
            Self::InlinePropertyBytesStateWithoutSchema => {
                write!(
                    f,
                    "label bucket inline property bytes state requires a non-zero byte width"
                )
            }
            Self::TinyTreeModeConflict => {
                write!(f, "label bucket tiny bit conflicts with tree mode")
            }
            Self::TinyDegreeOutOfRange => {
                write!(f, "label bucket tiny degree exceeds TINY_MAX_DEGREE")
            }
            Self::TinyUsedWidthOutOfRange => {
                write!(
                    f,
                    "label bucket tiny used width must be between degree and TINY_MAX_DEGREE"
                )
            }
            Self::TinyValueStatePresent => {
                write!(f, "label bucket tiny enable requires empty value state")
            }
            Self::SpillRunOutOfRange => {
                write!(
                    f,
                    "label bucket spill run id is out of the arena's range or is the sentinel"
                )
            }
            Self::SpillRunOnNonSlabBucket => {
                write!(
                    f,
                    "label bucket tiny/tree mode must not carry a spill run id"
                )
            }
        }
    }
}

impl std::error::Error for LabelBucketFieldError {}

impl CsrVertex for LabelBucket {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.edge_start()
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    fn stored_degree(&self) -> u32 {
        self.stored_slots()
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        self.try_with_edge_range(start, self.stored_slots())
            .expect("LabelBucket::with_base_slot_start: slot index overflow")
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }

    /// This bucket's edge-plane spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`].
    fn spill_run(self) -> u32 {
        self.edge_spill_run
    }

    fn try_with_spill_run(self, run: u32) -> Result<Self, ()> {
        self.try_with_edge_spill_run(run).map_err(|_| ())
    }

    fn trusts_neighbor_boundary(self) -> bool {
        // Label buckets are always viewed through a synthetic two-row accessor
        // (LabelEdgeSpanAccess). The slab-window end is the next bucket's
        // edge_start (or the vertex span end), never the PMA leaf block cap.
        true
    }

    fn after_slab_tombstone_delete(self) -> Self {
        self.with_degree(self.degree.saturating_sub(1))
    }

    fn try_grow_packed_slab_by_one(self) -> Result<Self, ()> {
        let next_degree = self.degree.checked_add(1).ok_or(())?;
        if self.edge_spill_run != NO_SPILL_RUN {
            // Spill-backed growth (ADR 0097 §7): the edge lives in the bucket's own run, not in
            // the slab window, so only the logical degree grows. `stored_slots` is the slab prefix
            // width, and leaving it alone is what keeps the spill ordinals (`prefix + i`) and every
            // reader's prefix derivation stable. The retired log head expressed this same branch;
            // it was dropped with the head, and the spill producer's publish needs it back.
            Ok(self.with_degree(next_degree))
        } else {
            // Packed slab append: the edge is appended inside the bucket's slab
            // window, so both logical degree and stored width grow.
            let next_stored = self.stored_slots().checked_add(1).ok_or(())?;
            Ok(self.with_degree(next_degree).with_stored_slots(next_stored))
        }
    }

    fn grow_packed_slab_by_one(self) -> Self {
        self.try_grow_packed_slab_by_one()
            .expect("LabelBucket::grow_packed_slab_by_one: degree overflow")
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        self.with_degree(self.degree.saturating_add(1))
    }
}

impl CsrEdge for LabelBucket {
    const BYTES: usize = LabelBucket::BYTES;

    fn read_from(bytes: &[u8]) -> Self {
        LabelBucket::read_from(bytes)
    }

    fn write_to(&self, bytes: &mut [u8]) {
        LabelBucket::write_to(*self, bytes);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(u32::from(self.bucket_label_key().raw()))
    }

    fn with_neighbor_vid(&self, _vid: VertexId) -> Self {
        *self
    }
}

/// [`LabeledVertex`] metadata layout in the upper 28 bits of [`LabeledVertex::locator`]:
///
/// ```text
/// bit 0      vertex tombstone (highest-priority scan gate)
/// bit 1      default-label bypass active
/// bit 2      bypass stores undirected homogeneous edges
/// bits 3–27  bypass mode: named reserve (ADR 0097 §3) - "reserve for the next bound widening,
///            not padding". The retired 8-bit bypass edge overflow-log head freed bits 4–11; bits
///            3 and 12–27 were already unused in bypass mode. It cannot raise the edge arena's
///            bound (the shared tail28 sets it), and the bypass run id lives in its own field.
/// bits 12–27 normal mode: LabelBucket descriptor slack beyond [`LabeledVertex::degree`] (`u16`)
/// ```
const VERTEX_TOMBSTONE_BIT: u32 = 1;
const DEFAULT_EDGE_LABELED_BIT: u32 = 1 << 1;
const BYPASS_UNDIRECTED_BIT: u32 = 1 << 2;
/// Bypass-mode metadata28 bits 3..28: named reserve (ADR 0097 §3), zero on the wire.
const BYPASS_METADATA_RESERVE_BITS: u32 = 25;
const BYPASS_METADATA_RESERVE_MASK: u32 = ((1 << BYPASS_METADATA_RESERVE_BITS) - 1) << 3;
const BUCKET_SLACK_SHIFT: u32 = 12;
const BUCKET_SLACK_BITS: u32 = 16;
const BUCKET_SLACK_MASK: u32 = ((1 << BUCKET_SLACK_BITS) - 1) << BUCKET_SLACK_SHIFT;

/// Maximum live [`LabelBucket`] rows per vertex (`BucketLabelKey` wire space size).
pub const MAX_VERTEX_LABEL_BUCKETS: u32 = u16::MAX as u32 + 1;

/// Maximum slack slots reserved past [`LabeledVertex::degree`] in metadata.
pub const MAX_VERTEX_LABEL_BUCKET_SLACK: u16 = u16::MAX;

/// Invalid [`LabeledVertex`] field combinations for normal (bucket) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabeledVertexFieldError {
    /// Normal-mode [`LabeledVertex::degree`] exceeds [`MAX_VERTEX_LABEL_BUCKETS`].
    LabelBucketCountOverflow,
    /// Descriptor span `degree + slack` does not fit in `u32`.
    LabelBucketDescriptorSpanOverflow,
    /// `base_slot_start` does not fit in the 36-bit slot index.
    SlotIndexOverflow,
    /// Bypass-mode metadata28 reserve bits are not zero on the wire.
    MetadataReservedBitsSet,
    /// A bypass spill run id is out of the shared arena's range or is the sentinel.
    SpillRunOutOfRange,
    /// A normal (bucket-mode) vertex carries a bypass spill run id, which only bypass rows may.
    SpillRunOnNormalRow,
    /// [`Self::inline_property_bytes_allocated_bytes`] does not fit in the 40-bit byte-offset space.
    ValueAllocatedBytesOverflow,
}

impl core::fmt::Display for LabeledVertexFieldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LabelBucketCountOverflow => write!(
                f,
                "label bucket row count exceeds MAX_VERTEX_LABEL_BUCKETS ({MAX_VERTEX_LABEL_BUCKETS})"
            ),
            Self::LabelBucketDescriptorSpanOverflow => {
                write!(
                    f,
                    "label bucket descriptor span (degree + slack) overflows u32"
                )
            }
            Self::SlotIndexOverflow => {
                write!(f, "vertex base_slot_start exceeds 36-bit slot index")
            }
            Self::MetadataReservedBitsSet => {
                write!(f, "vertex bypass metadata reserve bits must be zero")
            }
            Self::SpillRunOutOfRange => {
                write!(
                    f,
                    "vertex bypass spill run id is out of range or is the sentinel"
                )
            }
            Self::SpillRunOnNormalRow => {
                write!(
                    f,
                    "normal (bucket-mode) vertex must not carry a bypass spill run id"
                )
            }
            Self::ValueAllocatedBytesOverflow => {
                write!(
                    f,
                    "vertex inline_property_bytes_allocated_bytes exceeds 40-bit byte offset"
                )
            }
        }
    }
}

impl std::error::Error for LabeledVertexFieldError {}

/// Per-vertex locator for one labeled CSR orientation (21 bytes).
///
/// - **Normal:** [`Self::degree`] is the live [`LabelBucket`] row count (≤ [`MAX_VERTEX_LABEL_BUCKETS`]);
///   locator bits 36–63 hold [`Self::bucket_slack_slots`] so the physical descriptor span is
///   `degree + slack`; [`Self::stored_slots`] is the separate VertexEdgeSpan width for edge bytes.
/// - **Bypass:** [`Self::degree`] is the logical out-edge count (full `u32`); [`Self::stored_slots`]
///   is the stored slab width (tombstones included). The bypass spill run id lives in its own
///   field at bytes 21..25; metadata28 bits 3..27 are a named reserve in bypass mode (ADR 0097 §3).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabeledVertex {
    locator: u64,
    /// Logical out-degree or live label-bucket row count (mode-dependent).
    pub degree: u32,
    /// Stored edge-slab width for this vertex's VertexEdgeSpan (tombstones included).
    pub stored_slots: u32,
    /// Physical byte width of this vertex's value span in `EdgeInlinePropertyBytesStore` (slack included).
    inline_property_bytes_allocated_bytes: u64,
    /// Bypass-mode spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`]. 27 bits of this `u32` field are
    /// used (the shared arena's range, `ArenaRows::EDGE`); the top 5 bits are zero. A normal
    /// (bucket-mode) row carries no level-1 spill, so the field must be [`NO_SPILL_RUN`] there.
    bypass_spill_run: u32,
}

impl Default for LabeledVertex {
    /// All-zero row with no spill run: `bypass_spill_run` must default to [`NO_SPILL_RUN`], not to
    /// the derived `0`, which would read as a live run id.
    fn default() -> Self {
        Self::try_from_parts(0, 0, 0, 0, 0).expect("LabeledVertex::default: zero row")
    }
}

impl LabeledVertex {
    /// Fixed byte width of one encoded vertex row.
    ///
    /// ADR 0097 §3: 21 -> 25 bytes for the bypass spill run id, in its own `u32` field. The 25
    /// metadata28 bits the retired bypass edge log head freed are a named reserve, not scattered
    /// into the id (one packing helper per field, ADR 0096 §2).
    pub const BYTES: usize = 25;

    /// Builds a row from logical fields.
    #[inline]
    pub fn from_parts(
        base_slot_start: u64,
        degree: u32,
        stored_slots: u32,
        metadata28: u32,
    ) -> Self {
        Self::try_from_parts(base_slot_start, degree, stored_slots, 0, metadata28)
            .expect("LabeledVertex::from_parts: invalid fields")
    }

    /// Fallible constructor with release-safe range checks.
    #[inline]
    pub fn try_from_parts(
        base_slot_start: u64,
        degree: u32,
        stored_slots: u32,
        inline_property_bytes_allocated_bytes: u64,
        metadata28: u32,
    ) -> Result<Self, LabeledVertexFieldError> {
        if !slot_index_fits(base_slot_start) {
            return Err(LabeledVertexFieldError::SlotIndexOverflow);
        }
        if !byte_offset_fits(inline_property_bytes_allocated_bytes) {
            return Err(LabeledVertexFieldError::ValueAllocatedBytesOverflow);
        }
        if metadata28 & BYPASS_METADATA_RESERVE_MASK != 0
            && (metadata28 & DEFAULT_EDGE_LABELED_BIT) == 0
        {
            // Normal-mode rows own bits 12..27 as bucket slack; the reserve is bypass-only.
        }
        let locator = try_encode_locator_word(base_slot_start, metadata28)
            .ok_or(LabeledVertexFieldError::SlotIndexOverflow)?;
        Ok(Self {
            locator,
            degree,
            stored_slots,
            inline_property_bytes_allocated_bytes,
            bypass_spill_run: NO_SPILL_RUN,
        })
    }

    /// Physical byte width reserved for edge inline property bytes on this vertex.
    #[inline]
    pub fn inline_property_bytes_allocated_bytes(self) -> u64 {
        self.inline_property_bytes_allocated_bytes
    }

    /// Returns a copy with [`Self::inline_property_bytes_allocated_bytes`] updated.
    #[inline]
    pub fn with_inline_property_bytes_allocated_bytes(self, bytes: u64) -> Self {
        Self {
            inline_property_bytes_allocated_bytes: bytes,
            ..self
        }
    }

    /// Returns a copy with [`Self::inline_property_bytes_allocated_bytes`] updated, or an error if it does not fit.
    #[inline]
    pub fn try_with_inline_property_bytes_allocated_bytes(
        self,
        bytes: u64,
    ) -> Result<Self, LabeledVertexFieldError> {
        if !byte_offset_fits(bytes) {
            return Err(LabeledVertexFieldError::ValueAllocatedBytesOverflow);
        }
        Ok(self.with_inline_property_bytes_allocated_bytes(bytes))
    }

    /// Global label-bucket descriptor base (normal mode) or edge-slab base (bypass mode).
    #[inline]
    pub fn base_slot_start(self) -> u64 {
        decode_slot_index(self.locator)
    }

    /// Raw 28-bit metadata word in the locator (mode flags, slack, bypass log head, …).
    #[inline]
    pub fn metadata28(self) -> u32 {
        decode_meta28(self.locator)
    }

    #[inline]
    fn metadata_word(self) -> u32 {
        self.metadata28()
    }

    #[inline]
    fn with_locator(mut self, locator: u64) -> Self {
        self.locator = locator;
        self
    }

    #[inline]
    fn with_metadata_word(self, raw: u32) -> Self {
        self.with_locator(encode_locator_word(self.base_slot_start(), raw))
    }

    /// Returns `true` when this vertex points directly into the edge CSR.
    #[inline]
    pub fn is_default_edge_labeled(self) -> bool {
        (self.metadata_word() & DEFAULT_EDGE_LABELED_BIT) != 0
    }

    /// Returns a copy with the default-label bypass flag changed.
    #[inline]
    pub fn with_default_edge_labeled(self, enabled: bool) -> Self {
        let mut raw = self.metadata_word();
        if enabled {
            raw |= DEFAULT_EDGE_LABELED_BIT;
            raw &= !BUCKET_SLACK_MASK;
            raw &= !BYPASS_METADATA_RESERVE_MASK;
            self.with_metadata_word(raw)
                .with_degree(0)
                .with_stored_slots(0)
        } else {
            raw &= !DEFAULT_EDGE_LABELED_BIT;
            raw &= !BYPASS_UNDIRECTED_BIT;
            raw &= !BYPASS_METADATA_RESERVE_MASK;
            self.with_metadata_word(raw)
        }
    }

    /// Returns `true` when bypass mode stores undirected homogeneous edges (`label | 0x8000`).
    #[inline]
    pub fn is_bypass_undirected(self) -> bool {
        self.is_default_edge_labeled() && (self.metadata_word() & BYPASS_UNDIRECTED_BIT) != 0
    }

    /// Returns a copy with the bypass undirected flag changed (only meaningful in bypass mode).
    #[inline]
    pub fn with_bypass_undirected(self, undirected: bool) -> Self {
        let mut raw = self.metadata_word();
        if undirected {
            raw |= BYPASS_UNDIRECTED_BIT;
        } else {
            raw &= !BYPASS_UNDIRECTED_BIT;
        }
        self.with_metadata_word(raw)
    }

    /// Returns the storage label id for a homogeneous bypass row.
    #[inline]
    pub fn bypass_storage_label(self, default_label: BucketLabelKey) -> BucketLabelKey {
        debug_assert!(self.is_default_edge_labeled());
        if self.is_bypass_undirected() {
            BucketLabelKey::from_raw(default_label.raw() & BUCKET_LABEL_INDEX_MASK)
        } else {
            default_label
        }
    }

    /// Returns a copy configured for homogeneous default-label bypass.
    #[inline]
    pub fn with_homogeneous_bypass_label(self, label_key: BucketLabelKey) -> Self {
        self.with_default_edge_labeled(true)
            .with_bypass_undirected(label_key.is_undirected())
    }

    /// Returns `true` when the vertex row is a tombstone.
    #[inline]
    pub fn is_tombstone(self) -> bool {
        (self.metadata_word() & VERTEX_TOMBSTONE_BIT) != 0
    }

    /// Returns a copy with the tombstone flag changed.
    #[inline]
    pub fn with_tombstone(self, tomb: bool) -> Self {
        let mut raw = self.metadata_word();
        if tomb {
            raw |= VERTEX_TOMBSTONE_BIT;
        } else {
            raw &= !VERTEX_TOMBSTONE_BIT;
        }
        self.with_metadata_word(raw)
    }

    /// Extra LabelBucket descriptor slots reserved past live [`Self::degree`] (normal mode).
    #[inline]
    pub fn bucket_slack_slots(self) -> u16 {
        ((self.metadata_word() & BUCKET_SLACK_MASK) >> BUCKET_SLACK_SHIFT) as u16
    }

    /// Returns a copy with LabelBucket descriptor slack changed (normal mode only).
    #[inline]
    pub fn with_bucket_slack_slots(self, slack: u16) -> Self {
        let clamped = u32::from(slack);
        let mut raw = self.metadata_word() & !BUCKET_SLACK_MASK;
        raw |= clamped << BUCKET_SLACK_SHIFT;
        self.with_metadata_word(raw)
    }

    /// Physical LabelBucket descriptor span: [`Self::degree`] + [`Self::bucket_slack_slots`].
    #[inline]
    pub fn label_bucket_descriptor_span(self) -> Option<u32> {
        if self.is_default_edge_labeled() {
            return None;
        }
        self.degree()
            .checked_add(u32::from(self.bucket_slack_slots()))
    }

    /// Returns `true` when `count` is a valid normal-mode LabelBucket row count.
    #[inline]
    pub fn label_bucket_count_fits(count: u32) -> bool {
        count <= MAX_VERTEX_LABEL_BUCKETS
    }

    /// Slack for a physical descriptor span and live row count.
    #[inline]
    pub fn bucket_slack_for_descriptor_span(live_rows: u32, physical_span: u32) -> Option<u16> {
        let slack = physical_span.checked_sub(live_rows)?;
        u16::try_from(slack).ok()
    }

    /// Bypass-mode spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`] when nothing has spilled.
    #[inline]
    pub fn bypass_spill_run(self) -> u32 {
        self.bypass_spill_run
    }

    /// Fallible setter for the bypass spill run id: it must fit the shared arena's capacity
    /// ([`ArenaRows::EDGE`], the unlabeled tail28's 27-bit range) and must not be the sentinel.
    #[inline]
    pub fn try_with_bypass_spill_run(self, run: u32) -> Result<Self, LabeledVertexFieldError> {
        if SpillRunId::mint(run, ArenaRows::EDGE).is_err() {
            return Err(LabeledVertexFieldError::SpillRunOutOfRange);
        }
        Ok(Self {
            bypass_spill_run: run,
            ..self
        })
    }

    /// Returns a copy with the bypass spill run id set (bypass mode only).
    #[inline]
    pub fn with_bypass_spill_run(self, run: u32) -> Self {
        debug_assert!(self.is_default_edge_labeled());
        self.try_with_bypass_spill_run(run)
            .expect("LabeledVertex::with_bypass_spill_run: run id out of range")
    }

    /// Validates normal-mode label-bucket row count and descriptor span.
    #[inline]
    pub fn ensure_valid_normal_row(self) -> Result<Self, LabeledVertexFieldError> {
        if self.is_default_edge_labeled() {
            // ADR 0097 §3: the retired bypass edge log head left metadata28 bits 3..28 as a named
            // reserve; they are zero on the wire, and the bypass run id lives in its own field.
            if self.metadata_word() & BYPASS_METADATA_RESERVE_MASK != 0 {
                return Err(LabeledVertexFieldError::MetadataReservedBitsSet);
            }
            return Ok(self);
        }
        // A normal (bucket-mode) vertex carries no level-1 spill.
        if self.bypass_spill_run != NO_SPILL_RUN {
            return Err(LabeledVertexFieldError::SpillRunOnNormalRow);
        }
        if !Self::label_bucket_count_fits(self.degree) {
            return Err(LabeledVertexFieldError::LabelBucketCountOverflow);
        }
        if self.label_bucket_descriptor_span().is_none() {
            return Err(LabeledVertexFieldError::LabelBucketDescriptorSpanOverflow);
        }
        Ok(self)
    }

    /// Returns a copy with normal-mode label-bucket row count, or an error if it does not fit.
    #[inline]
    pub fn try_with_label_bucket_count(
        self,
        label_bucket_count: u32,
    ) -> Result<Self, LabeledVertexFieldError> {
        if self.is_default_edge_labeled() {
            return Ok(self.with_degree(label_bucket_count));
        }
        if !Self::label_bucket_count_fits(label_bucket_count) {
            return Err(LabeledVertexFieldError::LabelBucketCountOverflow);
        }
        Ok(self.with_degree(label_bucket_count))
    }

    /// Returns a copy with bucket locator fields updated together.
    #[inline]
    pub fn try_with_bucket_row(
        self,
        base_slot_start: u64,
        label_bucket_count: u32,
    ) -> Result<Self, LabeledVertexFieldError> {
        self.try_with_label_bucket_count(label_bucket_count)?
            .try_with_base_slot_start(base_slot_start)
    }

    /// Returns a copy with [`Self::base_slot_start`] updated, or an error if it does not fit.
    #[inline]
    pub fn try_with_base_slot_start(
        self,
        base_slot_start: u64,
    ) -> Result<Self, LabeledVertexFieldError> {
        let locator = try_replace_slot_index(self.locator, base_slot_start)
            .ok_or(LabeledVertexFieldError::SlotIndexOverflow)?;
        Ok(self.with_locator(locator))
    }

    /// Returns a copy with bucket locator, live count, and descriptor slack updated together.
    #[inline]
    pub fn try_with_bucket_row_and_slack(
        self,
        base_slot_start: u64,
        label_bucket_count: u32,
        bucket_slack_slots: u16,
    ) -> Result<Self, LabeledVertexFieldError> {
        self.try_with_bucket_row(base_slot_start, label_bucket_count)
            .map(|v| v.with_bucket_slack_slots(bucket_slack_slots))
            .and_then(LabeledVertex::ensure_valid_normal_row)
    }

    /// Returns a copy with bucket locator fields updated together.
    ///
    /// Panics in debug builds when `label_bucket_count` does not fit; use
    /// [`Self::try_with_bucket_row`] in release-safe paths.
    #[inline]
    pub fn with_bucket_row(self, base_slot_start: u64, label_bucket_count: u32) -> Self {
        self.try_with_bucket_row(base_slot_start, label_bucket_count)
            .expect("label bucket count overflow")
    }

    /// Returns a copy with bucket locator, live count, and descriptor slack updated together.
    ///
    /// Panics in debug builds on overflow; use [`Self::try_with_bucket_row_and_slack`] otherwise.
    #[inline]
    pub fn with_bucket_row_and_slack(
        self,
        base_slot_start: u64,
        label_bucket_count: u32,
        bucket_slack_slots: u16,
    ) -> Self {
        self.try_with_bucket_row_and_slack(base_slot_start, label_bucket_count, bucket_slack_slots)
            .expect("label bucket row overflow")
    }

    /// Returns a copy with [`Self::stored_slots`] updated.
    #[inline]
    pub fn with_stored_slots(mut self, slots: u32) -> Self {
        self.stored_slots = slots;
        self
    }

    /// Encodes this vertex row into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..8].copy_from_slice(&self.locator.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.degree.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.stored_slots.to_le_bytes());
        let value_alloc_wire: &mut [u8; 5] = (&mut bytes[16..21]).try_into().expect(
            "LabeledVertex inline_property_bytes_allocated_bytes wire slice must be 5 bytes",
        );
        write_u40(self.inline_property_bytes_allocated_bytes, value_alloc_wire);
        bytes[21..25].copy_from_slice(&self.bypass_spill_run.to_le_bytes());
    }

    /// Decodes a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        Self::try_read_from(bytes).expect("invalid LabeledVertex wire bytes")
    }

    /// Decodes and validates a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn try_read_from(bytes: &[u8]) -> Result<Self, LabeledVertexFieldError> {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabeledVertex::try_read_from expects exactly Self::BYTES bytes");
        let inline_property_bytes_allocated_bytes = read_u40(&chunk[16..21].try_into().unwrap());
        if !byte_offset_fits(inline_property_bytes_allocated_bytes) {
            return Err(LabeledVertexFieldError::ValueAllocatedBytesOverflow);
        }
        let vertex = Self {
            locator: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
            degree: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            stored_slots: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            inline_property_bytes_allocated_bytes,
            bypass_spill_run: u32::from_le_bytes(chunk[21..25].try_into().unwrap()),
        };
        vertex.ensure_valid_normal_row()
    }
}

impl CsrVertex for LabeledVertex {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        decode_slot_index(self.locator)
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    /// Layout width for [`crate::lara::edge::EdgeStore::insert_edge`]:
    /// bypass physical slab slots ([`Self::stored_slots`]), or normal live bucket rows ([`Self::degree`]).
    ///
    /// Normal-mode edge bytes are *not* sized by this value; they use [`Self::stored_slots`] on the
    /// vertex row and per-[`LabelBucket`] spans instead.
    fn stored_degree(&self) -> u32 {
        if self.is_default_edge_labeled() {
            self.stored_slots
        } else {
            self.degree
        }
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        self.try_with_base_slot_start(start)
            .expect("LabeledVertex::with_base_slot_start: slot index overflow")
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }

    /// This row's bypass spill run id (ADR 0097 §3), or [`NO_SPILL_RUN`].
    fn spill_run(self) -> u32 {
        self.bypass_spill_run
    }

    fn try_with_spill_run(self, run: u32) -> Result<Self, ()> {
        self.try_with_bypass_spill_run(run).map_err(|_| ())
    }

    fn slab_append_exclusive_end(self, base: u64) -> Option<u64> {
        if self.is_default_edge_labeled() {
            let end = checked_add_slot_index(base, u64::from(self.stored_slots))?;
            checked_add_slot_index(end, 1)
        } else {
            None
        }
    }

    fn after_slab_tombstone_delete(self) -> Self {
        if self.is_default_edge_labeled() {
            self.with_degree(self.degree.saturating_sub(1))
        } else {
            self
        }
    }

    fn try_grow_packed_slab_by_one(self) -> Result<Self, ()> {
        if self.is_default_edge_labeled() {
            let next_degree = self.degree.checked_add(1).ok_or(())?;
            let next_stored = self.stored_slots.checked_add(1).ok_or(())?;
            Ok(self.with_degree(next_degree).with_stored_slots(next_stored))
        } else {
            let next_degree = self.degree.checked_add(1).ok_or(())?;
            if !Self::label_bucket_count_fits(next_degree) {
                return Err(());
            }
            Ok(self.with_degree(next_degree))
        }
    }

    fn grow_packed_slab_by_one(self) -> Self {
        match self.try_grow_packed_slab_by_one() {
            Ok(grown) => grown,
            Err(()) => {
                debug_assert!(
                    false,
                    "grow_packed_slab_by_one: overflow (insert_edge should reject first)"
                );
                self
            }
        }
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        if self.is_default_edge_labeled() {
            self.with_degree(self.degree.saturating_add(1))
        } else {
            self
        }
    }
}

impl CsrVertexTombstone for LabeledVertex {
    fn is_tombstone(&self) -> bool {
        (*self).is_tombstone()
    }

    fn with_tombstone(self, tomb: bool) -> Self {
        LabeledVertex::with_tombstone(self, tomb)
    }
}

impl Storable for LabeledVertex {
    const BOUND: Bound = Bound::Bounded {
        max_size: LabeledVertex::BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; LabeledVertex::BYTES];
        self.write_to(&mut bytes);
        Cow::Owned(Vec::from(bytes))
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut bytes = [0u8; LabeledVertex::BYTES];
        self.write_to(&mut bytes);
        Vec::from(bytes)
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Self::try_read_from(bytes.as_ref())
            .expect("LabeledVertex stable bytes failed normal-row validation")
    }
}

impl Storable for LabelBucket {
    const BOUND: Bound = Bound::Bounded {
        max_size: LabelBucket::BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; LabelBucket::BYTES];
        self.write_to(&mut bytes);
        Cow::Owned(Vec::from(bytes))
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut bytes = [0u8; LabelBucket::BYTES];
        self.write_to(&mut bytes);
        Vec::from(bytes)
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Self::read_from(bytes.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::slot_index::SLOT_INDEX_MASK;
    use core::mem;

    #[test]
    fn wire_rows_match_documented_layout() {
        // ADR 0097 §3: LabeledVertex 21 -> 25 (the bypass run id), LabelBucket 29 -> 36
        // (interim; the final 34-byte layout waits for the values twin, slice 5).
        assert_eq!(LabeledVertex::BYTES, 25);
        assert_eq!(LabelBucket::BYTES, 36);
        assert!(mem::size_of::<LabeledVertex>() >= LabeledVertex::BYTES);
        assert!(mem::size_of::<LabelBucket>() >= LabelBucket::BYTES);
    }

    #[test]
    fn labeled_vertex_wire_bytes_golden() {
        let vertex = LabeledVertex::from_parts(42, 3, 9, 0);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let mut expected = [0u8; LabeledVertex::BYTES];
        expected[0..8].copy_from_slice(&42u64.to_le_bytes());
        expected[8..12].copy_from_slice(&3u32.to_le_bytes());
        expected[12..16].copy_from_slice(&9u32.to_le_bytes());
        // ADR 0097 §3: the row widened by four bytes for the bypass run id, which a normal-mode
        // row must carry as the sentinel.
        expected[21..25].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(bytes, expected);
    }

    /// A slab bucket with distinct values in every field the run-id setters must leave alone.
    fn populated_slab_bucket() -> LabelBucket {
        LabelBucket::from_parts_with_inline_property(
            BucketLabelKey::from_raw(0x1234),
            0x0F_FFFF_FFFE,
            7,
            11,
            3,
            0xAB_CDEF_0123,
            5,
            9,
            4,
        )
    }

    #[test]
    fn label_bucket_spill_run_ids_round_trip_at_the_extremes() {
        // `u32::MAX` is the "no run" sentinel; the edge id is bounded by the shared arena's 27-bit
        // range (ArenaRows::EDGE), the values id by its own u32 field.
        for run in [0u32, 1, (1 << 27) - 2] {
            let edge = populated_slab_bucket().with_edge_spill_run(run);
            assert_eq!(edge.edge_spill_run(), run);
            let mut bytes = [0u8; LabelBucket::BYTES];
            edge.write_to(&mut bytes);
            assert_eq!(LabelBucket::read_from(&bytes).edge_spill_run(), run);
        }
        for run in [0u32, 1, u32::MAX - 1] {
            let values = populated_slab_bucket().with_values_spill_run(run);
            assert_eq!(values.values_spill_run(), run);
            let mut bytes = [0u8; LabelBucket::BYTES];
            values.write_to(&mut bytes);
            assert_eq!(LabelBucket::read_from(&bytes).values_spill_run(), run);
        }
        // Both sides of the edge bound: the last legal id is accepted, the first illegal one and the
        // sentinel are refused (never masked).
        assert!(
            populated_slab_bucket()
                .try_with_edge_spill_run((1 << 27) - 1)
                .is_err()
        );
        assert!(
            populated_slab_bucket()
                .try_with_edge_spill_run(u32::MAX)
                .is_err()
        );
        assert!(
            populated_slab_bucket()
                .try_with_values_spill_run(u32::MAX)
                .is_err()
        );
        let fresh = populated_slab_bucket();
        assert_eq!(fresh.edge_spill_run(), NO_SPILL_RUN);
        assert_eq!(fresh.values_spill_run(), NO_SPILL_RUN);
    }

    #[test]
    fn setting_one_label_bucket_spill_run_leaves_every_other_field_alone() {
        let base = populated_slab_bucket();
        let mut base_bytes = [0u8; LabelBucket::BYTES];
        base.write_to(&mut base_bytes);
        // Bytes 8..29 hold the ipb fields and the values log bytes; the added region starts at 29.
        let untouched = base_bytes[8..29].to_vec();
        let edge = base.with_edge_spill_run(0x00AB_CDEF);
        assert_eq!(edge.values_spill_run(), NO_SPILL_RUN);
        assert_eq!(edge.degree(), base.degree());
        assert_eq!(edge.stored_slots_raw(), base.stored_slots_raw());
        assert_eq!(edge.is_tiny_mode(), base.is_tiny_mode());
        assert_eq!(edge.is_tree_mode(), base.is_tree_mode());
        assert_eq!(edge.bucket_label_key(), base.bucket_label_key());
        assert_eq!(edge.edge_start(), base.edge_start());
        assert_eq!(
            edge.inline_property_bytes_log_head(),
            base.inline_property_bytes_log_head()
        );
        assert_eq!(
            edge.inline_property_bytes_log_len(),
            base.inline_property_bytes_log_len()
        );
        let mut edge_bytes = [0u8; LabelBucket::BYTES];
        edge.write_to(&mut edge_bytes);
        assert_eq!(edge_bytes[8..29], untouched[..]);
        let values = base.with_values_spill_run(0x0BAD_F00D);
        assert_eq!(values.edge_spill_run(), NO_SPILL_RUN);
        let mut values_bytes = [0u8; LabelBucket::BYTES];
        values.write_to(&mut values_bytes);
        assert_eq!(values_bytes[8..29], untouched[..]);
    }

    #[test]
    fn tiny_payload_survives_the_spill_run_fields() {
        // ADR 0096 §1 keeps tiny's payload meaning: the added region (bytes 29..36) is outside the
        // payload range (12..28), and a tiny bucket must carry no run id.
        let tiny = LabelBucket::from_parts(BucketLabelKey::from_raw(0x1234), 0x0F_FFFF_FFFE, 2, 2)
            .try_enable_tiny_mode()
            .expect("tiny")
            .with_tiny_target(0, 0xAABB_CCDD)
            .with_tiny_target(1, 0x1122_3344)
            .with_tiny_target(2, 0x5566_7788)
            .with_tiny_target(3, 0x99AA_BBCC)
            .with_tiny_used_width(3);
        let mut bytes = [0u8; LabelBucket::BYTES];
        tiny.write_to(&mut bytes);
        let back = LabelBucket::read_from(&bytes);
        assert!(back.is_tiny_mode());
        for (index, expected) in [0xAABB_CCDDu32, 0x1122_3344, 0x5566_7788, 0x99AA_BBCC]
            .into_iter()
            .enumerate()
        {
            assert_eq!(back.tiny_target(index as u32), expected);
        }
        assert_eq!(back.tiny_used_width(), 3);
        assert_eq!(back.edge_spill_run(), NO_SPILL_RUN);
        assert_eq!(back.values_spill_run(), NO_SPILL_RUN);
        // A wire row that carries a run id on a tiny bucket is rejected.
        bytes[29] = 1;
        assert_eq!(
            LabelBucket::try_read_from(&bytes),
            Err(LabelBucketFieldError::SpillRunOnNonSlabBucket)
        );
    }

    #[test]
    fn label_bucket_wire_bytes_golden() {
        let bucket =
            LabelBucket::from_parts(BucketLabelKey::from_raw(0x1234), 0x0F_FFFF_FFFE, 5, 9);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(bucket.edge_start(), 0x0F_FFFF_FFFE);
        assert_eq!(bucket.bucket_label_key().raw(), 0x1234);
        assert_eq!(bytes[8..12], 5u32.to_le_bytes());
        assert_eq!(bytes[12..16], 9u32.to_le_bytes());
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
    }

    #[test]
    fn try_from_parts_rejects_out_of_range_fields() {
        assert_eq!(
            LabelBucket::try_from_parts(
                BucketLabelKey::default(),
                SLOT_INDEX_MASK + 1,
                0,
                0,
                0u16,
                0,
                0,
                -1,
                0
            ),
            Err(LabelBucketFieldError::SlotIndexOverflow)
        );
        assert_eq!(
            LabeledVertex::try_from_parts(SLOT_INDEX_MASK + 1, 0, 0, 0, 0),
            Err(LabeledVertexFieldError::SlotIndexOverflow)
        );
        // ADR 0097 §3: the retired bypass edge log head left metadata28 bits 3..28 as a named
        // reserve that must be zero on a bypass row (a normal row owns those bits as bucket slack,
        // so `try_from_parts` accepts them there and `ensure_valid_normal_row` is the validator).
        assert_eq!(
            LabeledVertex::try_from_parts(0, 0, 0, 0, DEFAULT_EDGE_LABELED_BIT | (1 << 3))
                .expect("bypass row with reserve bits builds")
                .ensure_valid_normal_row(),
            Err(LabeledVertexFieldError::MetadataReservedBitsSet)
        );
    }

    #[test]
    fn try_with_base_slot_start_rejects_slot_overflow() {
        let vertex = LabeledVertex::default();
        let err = vertex
            .try_with_base_slot_start(SLOT_INDEX_MASK + 1)
            .expect_err("slot overflow");
        assert_eq!(err, LabeledVertexFieldError::SlotIndexOverflow);
    }

    #[test]
    fn label_bucket_round_trips_exact_layout() {
        let bucket =
            LabelBucket::from_parts(BucketLabelKey::from_raw(0x1234), 0x0F_FFFF_FFFE, 5, 9);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
        assert_eq!(bucket.base_slot_start(), bucket.edge_start());
        assert!(bucket.stored_slots() >= bucket.degree());
    }

    #[test]
    fn label_bucket_tiny_bit_takes_tiny_validation_not_reserved() {
        // ADR 0096 §1: the tiny flag (word bit 52 since ADR 0097 §3) selects tiny mode, so the
        // tiny rules judge the row. Byte 28 carries the K=4 used width,
        // which a fresh slab descriptor leaves at 0 — valid for an empty bucket;
        // an out-of-range used width must fail with the precise tiny error.
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 0, 0);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        bytes[6] |= 0x10; // word bit 52 (ADR 0097 §3 moved the tiny flag)
        assert!(LabelBucket::try_read_from(&bytes).is_ok());
        bytes[28] = 9; // used width above TINY_MAX_DEGREE
        assert_eq!(
            LabelBucket::try_read_from(&bytes),
            Err(LabelBucketFieldError::TinyUsedWidthOutOfRange)
        );
    }

    #[test]
    fn label_bucket_tree_mode_bit_round_trips() {
        // Plan 0318 §Step 1: bit 63 round-trips through encode/decode.
        let bucket =
            LabelBucket::from_parts(BucketLabelKey::default(), 0, 0, 0).with_tree_mode(true);
        assert!(bucket.is_tree_mode());
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        // bit 63 = high bit of byte 7 = 0x80.
        assert_eq!(bytes[7] & 0x80, 0x80, "bit 63 must be set in wire bytes");
        let decoded = LabelBucket::try_read_from(&bytes).expect("tree-mode bucket must decode");
        assert!(decoded.is_tree_mode(), "decoded bucket must be tree-mode");
        assert_eq!(decoded, bucket, "round-trip preserves all fields");
    }

    #[test]
    fn label_bucket_tree_mode_bit_cleared_default() {
        // Plan 0318 §Step 1: default `LabelBucket` is slab-mode (bit 63 = 0).
        let bucket = LabelBucket::default();
        assert!(!bucket.is_tree_mode(), "default bucket must be slab-mode");
        // `with_tree_mode(false)` is a no-op on a default bucket.
        let still_default = bucket.with_tree_mode(false);
        assert!(!still_default.is_tree_mode());
        assert_eq!(still_default, bucket);
    }

    #[test]
    fn label_bucket_tree_mode_bit_accepted_by_validator() {
        // The tree flag (word bit 53 since ADR 0097 §3) is not a reserved-bit trigger: a row
        // built with it set must round-trip through encode/decode and report tree mode.
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 0, 0);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        bytes[6] |= 0x20; // set word bit 53 = tree-mode flag (ADR 0097 §3 moved it)
        let decoded =
            LabelBucket::try_read_from(&bytes).expect("the tree-mode flag must be accepted");
        assert!(decoded.is_tree_mode());
    }

    #[test]
    fn label_bucket_with_tree_mode_toggles_and_preserves_other_fields() {
        // Plan 0318 §Step 1: `with_tree_mode` is a single-bit mutation that
        // preserves `bucket_label_key`, `edge_start`, `degree`, `stored_slots`,
        // and `inline_property_bytes_*`.
        let original = LabelBucket::from_parts_with_inline_property(
            BucketLabelKey::from_raw(0xABCD),
            0x1234_5678,
            7,
            11,
            32,
            0x0000_1234_5678, // u40 range: < 2^40 = 0x100_0000_0000
            9,
            2,
            4,
        );
        let tree = original.with_tree_mode(true);
        assert!(tree.is_tree_mode());
        assert_eq!(tree.bucket_label_key().raw(), 0xABCD);
        assert_eq!(tree.edge_start(), 0x1234_5678);
        assert_eq!(tree.degree, 7);
        assert_eq!(tree.stored_slots(), 11);
        assert_eq!(tree.inline_property_byte_width(), 32);
        assert_eq!(tree.inline_property_bytes_slab_slots(), 9);
        assert_eq!(tree.inline_property_bytes_log_len(), 4);

        let slab_again = tree.with_tree_mode(false);
        assert!(!slab_again.is_tree_mode());
        assert_eq!(slab_again, original);
    }

    #[test]
    fn label_bucket_round_trips_w64_inline_property_byte_width() {
        let bucket = LabelBucket::from_parts_with_inline_property(
            BucketLabelKey::default(),
            0,
            0,
            0,
            64u16,
            0,
            0,
            -1,
            0,
        );
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        let decoded = LabelBucket::try_read_from(&bytes).expect("decode");
        assert_eq!(decoded.inline_property_byte_width(), 64u16);
        assert_eq!(decoded.inline_property_byte_width(), 64);
    }

    #[test]
    fn labeled_vertex_round_trips_default_bypass_and_tombstone_bits() {
        let vertex = LabeledVertex::from_parts(42, 3, 9, 0)
            .with_default_edge_labeled(true)
            .with_bypass_undirected(true)
            .with_tombstone(true);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let decoded = LabeledVertex::read_from(&bytes);
        assert_eq!(decoded.degree, 0);
        assert_eq!(decoded.stored_slots, 0);
        assert!(decoded.is_default_edge_labeled());
        assert!(decoded.is_bypass_undirected());
        assert_eq!(
            decoded.bypass_storage_label(BucketLabelKey::UNLABELED_DIRECTED),
            BucketLabelKey::UNLABELED_UNDIRECTED
        );
        assert!(decoded.is_tombstone());
        assert_eq!(decoded.bypass_spill_run(), NO_SPILL_RUN);
        let with_run = decoded.with_bypass_spill_run(42);
        assert_eq!(with_run.bypass_spill_run(), 42);
        assert_eq!(decoded.bypass_spill_run(), NO_SPILL_RUN);
        let normal = decoded
            .with_default_edge_labeled(false)
            .with_degree(2)
            .with_stored_slots(0x1234);
        assert_eq!(normal.degree, 2);
        assert_eq!(normal.stored_slots, 0x1234);
        let with_slack = normal.with_bucket_slack_slots(37);
        assert_eq!(with_slack.bucket_slack_slots(), 37);
        assert_eq!(with_slack.label_bucket_descriptor_span(), Some(39));
        assert!(!with_slack.is_default_edge_labeled());
    }

    #[test]
    fn label_bucket_inline_property_bytes_offset_round_trips_on_wire() {
        let bucket = LabelBucket::from_parts_with_inline_property(
            BucketLabelKey::from_raw(2),
            10,
            2,
            2,
            2u16,
            4,
            2,
            -1,
            0,
        );
        assert_eq!(bucket.inline_property_bytes_offset(), 4);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(bytes[20], 4);
        let decoded = LabelBucket::read_from(&bytes);
        assert_eq!(decoded.inline_property_bytes_offset(), 4);
    }

    #[test]
    fn bucket_slack_metadata_is_u16_wide() {
        let vertex = LabeledVertex::default().with_bucket_slack_slots(u16::MAX);
        assert_eq!(vertex.bucket_slack_slots(), u16::MAX);
        let raw = vertex.metadata28();
        assert_eq!((raw >> 12) & 0xFFFF, u32::from(u16::MAX));
    }

    #[test]
    fn label_bucket_descriptor_span_is_degree_plus_slack() {
        let vertex = LabeledVertex::default()
            .with_degree(5)
            .with_bucket_slack_slots(10);
        assert_eq!(vertex.label_bucket_descriptor_span(), Some(15));
    }

    #[test]
    fn label_bucket_count_fits_matches_wire_space() {
        assert!(LabeledVertex::label_bucket_count_fits(
            MAX_VERTEX_LABEL_BUCKETS
        ));
        assert!(!LabeledVertex::label_bucket_count_fits(
            MAX_VERTEX_LABEL_BUCKETS + 1
        ));
    }

    #[test]
    fn try_with_label_bucket_count_rejects_overflow_in_release() {
        let vertex = LabeledVertex::default();
        let err = vertex
            .try_with_label_bucket_count(MAX_VERTEX_LABEL_BUCKETS + 1)
            .expect_err("overflow must be rejected");
        assert_eq!(err, LabeledVertexFieldError::LabelBucketCountOverflow);
    }

    #[test]
    fn try_read_from_rejects_normal_row_with_overflow_degree() {
        let vertex = LabeledVertex::default().with_degree(MAX_VERTEX_LABEL_BUCKETS + 1);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let err = LabeledVertex::try_read_from(&bytes).expect_err("wire row must be rejected");
        assert_eq!(err, LabeledVertexFieldError::LabelBucketCountOverflow);
    }

    #[test]
    fn label_bucket_tombstone_delete_keeps_physical_width() {
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 2, 5)
            .after_slab_tombstone_delete();
        assert_eq!(bucket.degree, 1);
        assert_eq!(bucket.stored_slots(), 5);
    }

    // ADR 0096 §1 (R2a): inline-tiny wire rules. No dispatch arms exist yet, so
    // these tests pin the wire contract the R2b arms will rely on.

    fn tiny_bucket_degree2() -> LabelBucket {
        LabelBucket::from_parts(BucketLabelKey::from_raw(5), 100, 2, 2)
            .try_enable_tiny_mode()
            .expect("fresh degree-2 bucket enables tiny")
            .with_tiny_target(0, 7)
            .with_tiny_target(1, 9)
    }

    #[test]
    fn tiny_wire_bytes_golden() {
        let bucket = tiny_bucket_degree2();
        assert!(bucket.is_tiny_mode());
        assert!(!bucket.is_tree_mode());
        assert_eq!(bucket.degree, 2);
        assert_eq!(bucket.stored_slots(), 2);
        assert_eq!(bucket.tiny_target(0), 7);
        assert_eq!(bucket.tiny_target(1), 9);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        let word = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        assert_eq!(word & 0xFFFF_FFFF, 100); // anchor preserved
        assert_eq!(((word >> 36) & 0xFFFF) as u16, 5); // label intact
        // ADR 0097 §3 layout: bit 52 tiny, bit 53 tree, bits 54..64 the edge run id's low bits
        // (all ones = the "no run" sentinel).
        assert_ne!(word & (1u64 << 52), 0); // tiny bit
        assert_eq!(word & (1u64 << 53), 0); // tree bit clear
        assert_eq!((word >> 54) & 0x3FF, 0x3FF); // run low bits = NO_SPILL_RUN
        assert_eq!(bytes[8..12], 2u32.to_le_bytes());
        assert_eq!(bytes[12..16], 0u32.to_le_bytes()); // T3 zeroed at enable
        assert_eq!(bytes[16..20], 7u32.to_le_bytes()); // T0
        assert_eq!(bytes[20..24], 9u32.to_le_bytes()); // T1
        assert!(bytes[24..28].iter().all(|&b| b == 0)); // T2 zero
        assert_eq!(bytes[28], 2); // used width (K=4 byte-28 rule)
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
    }

    /// Plan 0361 R4 / ADR 0096 §3b Phase 1: the mode-aware accessor is the seam
    /// K=4 will repurpose. Slab and tree buckets report their raw stored width;
    /// tiny buckets report their live count (today the wire stores the live prefix
    /// width — holes included — in the raw field).
    ///
    /// Wrong implementation this fails on: an accessor that returns the raw field
    /// for tiny (the pre-R4 shape would report the prefix width, e.g. 3 with a
    /// tombstone hole, instead of the live 2).
    #[test]
    fn stored_slots_accessor_is_mode_aware() {
        let live = LabelBucket::from_parts(BucketLabelKey::from_raw(5), 200, 3, 3)
            .try_enable_tiny_mode()
            .expect("enable tiny");
        assert_eq!(
            live.stored_slots(),
            3,
            "tiny accessor reports the live count"
        );

        // K=4: the raw field is T3 payload, so a missed dispatch must degrade to a
        // bounded live count instead of reading a target as a width (the T3 value
        // here is far outside any width).
        let holed = live.with_tiny_target(3, 1 << 20).with_degree_field(2);
        assert_eq!(holed.stored_slots_raw(), 1 << 20, "T3 carries the target");
        assert_eq!(holed.stored_slots(), 2, "accessor stays the live count");
        assert!(
            holed.stored_slots() <= LabelBucket::TINY_MAX_DEGREE,
            "miss-degradation bound"
        );

        // Slab and tree buckets keep the raw width in both accessors.
        let slab = LabelBucket::from_parts(BucketLabelKey::from_raw(5), 10, 2, 7);
        assert_eq!(slab.stored_slots_raw(), 7);
        assert_eq!(slab.stored_slots(), 7);
        let tree = slab.with_tree_mode(true);
        assert_eq!(tree.stored_slots_raw(), 7);
        assert_eq!(tree.stored_slots(), 7);
    }

    #[test]
    fn tiny_roundtrip_degrees_0_to_4() {
        for degree in 0..=4u32 {
            let mut bucket =
                LabelBucket::from_parts(BucketLabelKey::from_raw(5), 200, degree, degree)
                    .try_enable_tiny_mode()
                    .expect("fresh bucket enables tiny");
            for i in 0..degree {
                bucket = bucket.with_tiny_target(i, 1000 + i);
            }
            let mut bytes = [0u8; LabelBucket::BYTES];
            bucket.write_to(&mut bytes);
            let back = LabelBucket::try_read_from(&bytes).expect("tiny round-trip");
            assert_eq!(back, bucket);
            assert!(back.is_tiny_mode());
            for i in 0..degree {
                assert_eq!(back.tiny_target(i), 1000 + i);
            }
        }
    }

    #[test]
    fn tiny_target_t2_splits_across_three_fields() {
        // T2 = offset-hi byte | width LE | log byte — byte-exact composition.
        let bucket = LabelBucket::from_parts(BucketLabelKey::from_raw(5), 300, 3, 3)
            .try_enable_tiny_mode()
            .expect("enable")
            .with_tiny_target(2, 0xAABBCCDD);
        assert_eq!(bucket.tiny_target(2), 0xAABBCCDD);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(bytes[24], 0xDD);
        assert_eq!(bytes[25..27], 0xBBCCu16.to_le_bytes());
        assert_eq!(bytes[27], 0xAA);
        assert_eq!(bytes[28], 3); // used width equals degree 3
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
    }

    #[test]
    fn try_enable_tiny_mode_is_idempotent() {
        let bucket = tiny_bucket_degree2();
        let again = bucket.try_enable_tiny_mode().expect("re-enable");
        assert_eq!(again, bucket);
    }

    #[test]
    fn try_enable_tiny_mode_rejects_each_rule() {
        // Tree bit set.
        let tree = LabelBucket::from_parts(BucketLabelKey::default(), 0, 1, 1).with_tree_mode(true);
        assert_eq!(
            tree.try_enable_tiny_mode(),
            Err(LabelBucketFieldError::TinyTreeModeConflict)
        );
        // Degree 4 is the K=4 cap: accepted.
        let cap = LabelBucket::from_parts(BucketLabelKey::default(), 0, 4, 4)
            .try_enable_tiny_mode()
            .expect("degree 4 enables tiny");
        assert_eq!(cap.tiny_used_width(), 4);
        // Degree above the K=4 cap.
        let wide = LabelBucket::from_parts(BucketLabelKey::default(), 0, 5, 5);
        assert_eq!(
            wide.try_enable_tiny_mode(),
            Err(LabelBucketFieldError::TinyDegreeOutOfRange)
        );
        // Live value state (width).
        let valued = LabelBucket::from_parts_with_inline_property(
            BucketLabelKey::default(),
            0,
            1,
            1,
            4,
            0,
            0,
            -1,
            0,
        );
        assert_eq!(
            valued.try_enable_tiny_mode(),
            Err(LabelBucketFieldError::TinyValueStatePresent)
        );
        // Used width below degree (memory + wire sides): a live slot outside the
        // used range is an incoherent row.
        let incoherent = LabelBucket::from_parts(BucketLabelKey::default(), 0, 1, 1)
            .try_enable_tiny_mode()
            .expect("enable")
            .with_degree_field(2);
        assert_eq!(
            incoherent.try_enable_tiny_mode(),
            Err(LabelBucketFieldError::TinyUsedWidthOutOfRange)
        );
        let mut bytes = [0u8; LabelBucket::BYTES];
        incoherent.write_to(&mut bytes);
        assert_eq!(
            LabelBucket::try_read_from(&bytes),
            Err(LabelBucketFieldError::TinyUsedWidthOutOfRange)
        );
    }

    #[test]
    fn try_read_from_rejects_each_tiny_rule() {
        // Valid tiny degree-1 bytes; corrupt one rule at a time.
        fn valid_tiny_bytes() -> [u8; LabelBucket::BYTES] {
            let bucket = LabelBucket::from_parts(BucketLabelKey::from_raw(5), 100, 1, 1)
                .try_enable_tiny_mode()
                .expect("enable")
                .with_tiny_target(0, 11)
                .with_tiny_used_width(1);
            let mut bytes = [0u8; LabelBucket::BYTES];
            bucket.write_to(&mut bytes);
            bytes
        }
        // Tiny ∧ tree (word bits 52 and 53, byte 6).
        let mut bad = valid_tiny_bytes();
        bad[6] |= 0x20;
        assert_eq!(
            LabelBucket::try_read_from(&bad),
            Err(LabelBucketFieldError::TinyTreeModeConflict)
        );
        // Degree 5 (> K=4 cap).
        let mut bad = valid_tiny_bytes();
        bad[8..12].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(
            LabelBucket::try_read_from(&bad),
            Err(LabelBucketFieldError::TinyDegreeOutOfRange)
        );
        // Degree 4 is the cap: accepted (T3 payload is unconstrained).
        let mut ok = valid_tiny_bytes();
        ok[8..12].copy_from_slice(&4u32.to_le_bytes());
        ok[28] = 4; // used width must cover the live count
        assert!(LabelBucket::try_read_from(&ok).is_ok());
        // Used width below degree (live slot outside the used range).
        let mut bad = valid_tiny_bytes();
        bad[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            LabelBucket::try_read_from(&bad),
            Err(LabelBucketFieldError::TinyUsedWidthOutOfRange)
        );
        // T1 is free payload now (K=4): a nonzero value past degree is accepted.
        let mut ok = valid_tiny_bytes();
        ok[20..24].copy_from_slice(&5u32.to_le_bytes());
        assert!(LabelBucket::try_read_from(&ok).is_ok());
        // Byte 28 above the cap.
        let mut bad = valid_tiny_bytes();
        bad[28] = 7;
        assert_eq!(
            LabelBucket::try_read_from(&bad),
            Err(LabelBucketFieldError::TinyUsedWidthOutOfRange)
        );
    }
}
