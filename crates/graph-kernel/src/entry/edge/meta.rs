//! Compact metadata stored beside an edge target.
//!
//! [`EdgeMeta`] is the hot, fixed-width portion of an edge record. It keeps the
//! fields that are needed during traversal in a single `u32`, which lets an
//! [`crate::entry::edge::Edge`] remain exactly 8 bytes while still carrying
//! direction, placement, label, and small sidecar hints.
//!
//! Layout (little-endian on wire):
//!
//! ```text
//! 31                               0
//! +--------+----------+------------+
//! | flags  | sidecar  |  label_id  |
//! |  8bit  |   8bit   |   16bit    |
//! +--------+----------+------------+
//! ```
//!
//! The packed fields are:
//!
//! - `label_id` (`bits 0..=15`): the edge label identifier.
//! - `sidecar` (`bits 16..=23`): an inline byte interpreted according to
//!   [`SideCarKind`].
//! - `flags` (`bits 24..=31`): [`EdgeFlags`], including direction, remote
//!   placement, and the two-bit sidecar kind.
//!
//! Use [`EdgeMeta::to_le_bytes`] and [`EdgeMeta::from_le_bytes`] when crossing
//! a storage or wire boundary. [`EdgeMeta::raw`] exposes the in-memory packed
//! value for low-level callers that already know the layout.

use crate::entry::label::LabelId;
use bitflags::bitflags;

/// Hot metadata packed into 32 bits for one edge record.
///
/// The type is a transparent wrapper around `u32`, but callers should prefer
/// the accessor and constructor methods over depending on the raw layout. The
/// raw layout is documented at the module level for serialization, debugging,
/// and compatibility work.
///
/// `Default` produces a zeroed metadata word: no flags, a `0` sidecar byte, and
/// label id `0`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeMeta(u32);

impl EdgeMeta {
    const FLAGS_SHIFT: u32 = 24;

    /// Packs flags, a sidecar byte, and a label identifier into an [`EdgeMeta`].
    ///
    /// The supplied [`EdgeFlags`] are stored in the high byte, `sidecar` is
    /// stored in the middle byte, and `label_id` occupies the low 16 bits.
    /// No validation is performed on the relationship between the sidecar byte
    /// and the sidecar kind encoded in `flags`; higher layers are responsible
    /// for keeping those fields consistent.
    #[inline]
    pub const fn new(flags: EdgeFlags, sidecar: u8, label_id: LabelId) -> Self {
        Self((label_id.raw() as u32) | ((sidecar as u32) << 16) | ((flags.bits() as u32) << 24))
    }

    /// Returns the high-byte flag field.
    ///
    /// Unknown or currently reserved bits are retained so that older code can
    /// round-trip metadata produced by newer code without clearing those bits.
    #[inline]
    pub const fn flags(self) -> EdgeFlags {
        EdgeFlags::from_bits_retain(((self.0 >> 24) & 0xFF) as u8)
    }

    /// Returns the inline sidecar byte.
    ///
    /// Interpret this byte using [`Self::sidecar_kind`]. For
    /// [`SideCarKind::None`], callers should treat the value as unused.
    #[inline]
    pub const fn sidecar(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }

    /// Returns the interpretation assigned to the sidecar byte.
    ///
    /// The kind is encoded in bits 2 and 3 of [`EdgeFlags`], not in the sidecar
    /// byte itself.
    #[inline]
    pub fn sidecar_kind(self) -> SideCarKind {
        SideCarKind::from_flags(self.flags())
    }

    /// Returns the low 16-bit label identifier.
    ///
    /// The return type is `u16` because the packed representation stores only
    /// the numeric id. Convert at the call site when a typed [`LabelId`] is
    /// required.
    #[inline]
    pub const fn label_id(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// Returns the packed `u32` value in host-endian integer form.
    ///
    /// This is useful for diagnostics and low-level indexing. For portable
    /// serialization, use [`Self::to_le_bytes`] instead.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Serializes the packed metadata word as four little-endian bytes.
    ///
    /// This preserves the documented wire layout independent of the host CPU's
    /// native byte order.
    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Deserializes metadata from the documented four-byte little-endian form.
    ///
    /// Reserved and unknown flag bits are preserved; they can be inspected
    /// through [`Self::flags`] or round-tripped with [`Self::to_le_bytes`].
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }

    /// Returns `true` when the edge should be interpreted as undirected.
    ///
    /// This is a convenience wrapper around [`EdgeFlags::UNDIRECTED`].
    #[inline]
    pub const fn is_undirected(self) -> bool {
        self.flags().contains(EdgeFlags::UNDIRECTED)
    }

    /// Returns a copy of this metadata with the undirected flag set or cleared.
    ///
    /// All other fields, including reserved flag bits, are preserved.
    #[inline]
    pub const fn with_undirected(self, undirected: bool) -> Self {
        self.with_flag(EdgeFlags::UNDIRECTED, undirected)
    }

    /// Returns `true` when the edge target lives outside the local partition.
    ///
    /// This is a convenience wrapper around [`EdgeFlags::REMOTE`].
    #[inline]
    pub const fn is_remote(self) -> bool {
        self.flags().contains(EdgeFlags::REMOTE)
    }

    /// Returns a copy of this metadata with the remote flag set or cleared.
    ///
    /// All other fields, including reserved flag bits, are preserved.
    #[inline]
    pub const fn with_remote(self, remote: bool) -> Self {
        self.with_flag(EdgeFlags::REMOTE, remote)
    }

    #[inline]
    const fn with_flag(self, flag: EdgeFlags, enabled: bool) -> Self {
        let flag_bits = (flag.bits() as u32) << Self::FLAGS_SHIFT;

        if enabled {
            Self(self.0 | flag_bits)
        } else {
            Self(self.0 & !flag_bits)
        }
    }
}

bitflags! {
    /// Flags stored in the high byte of [`EdgeMeta`].
    ///
    /// Bits 0 and 1 describe edge behavior directly. Bits 2 and 3 encode the
    /// [`SideCarKind`] for the inline sidecar byte. The remaining bits are
    /// reserved for future payload, layout, and version information.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct EdgeFlags: u8 {
        /// The edge should be traversed as undirected.
        ///
        /// When absent, the edge is treated as directed according to the owning
        /// adjacency structure.
        const UNDIRECTED = 1 << 0;

        /// The edge target is stored in another canister or partition.
        ///
        /// Local traversal code can use this flag to route lookups through the
        /// remote edge path instead of assuming the target vertex is resident.
        const REMOTE = 1 << 1;

        /// Low bit of the two-bit [`SideCarKind`] field.
        ///
        /// Prefer [`SideCarKind::apply`] and [`SideCarKind::from_flags`] over
        /// setting this bit directly.
        const SIDECAR_KIND_0 = 1 << 2;
        /// High bit of the two-bit [`SideCarKind`] field.
        ///
        /// Prefer [`SideCarKind::apply`] and [`SideCarKind::from_flags`] over
        /// setting this bit directly.
        const SIDECAR_KIND_1 = 1 << 3;

        /// Reserved for a future payload or layout kind bit.
        const RESERVED_4 = 1 << 4;
        /// Reserved for a future payload or layout kind bit.
        const RESERVED_5 = 1 << 5;

        /// Reserved for future versioning or compatibility metadata.
        const RESERVED_6 = 1 << 6;
        /// Reserved for future versioning or compatibility metadata.
        const RESERVED_7 = 1 << 7;
    }
}

/// Meaning assigned to the one-byte sidecar field in [`EdgeMeta`].
///
/// The value is encoded in bits 2 and 3 of [`EdgeFlags`]. The sidecar byte is
/// intentionally generic; this enum tells readers how to interpret that byte
/// without expanding the fixed-width edge record.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideCarKind {
    /// No sidecar value is attached.
    ///
    /// The sidecar byte should be ignored by readers.
    None = 0,
    /// The sidecar byte stores a compact, quantized edge weight.
    ///
    /// The exact quantization scale is defined by the layer that creates the
    /// edge metadata.
    QuantizedWeight = 1,
    /// The sidecar byte stores a small rank or ordering hint.
    ///
    /// Traversal and ranking code can use this as a cheap local priority signal.
    RankHint = 2,
    /// The sidecar byte stores a compact recency bucket.
    ///
    /// This supports age-aware heuristics without adding a full timestamp to
    /// each edge record.
    RecencyBucket = 3,
}

impl SideCarKind {
    const MASK: u8 = 0b11;

    /// Decodes a sidecar kind from the sidecar-kind bits in [`EdgeFlags`].
    ///
    /// Other flag bits are ignored. Because the kind occupies two bits, every
    /// possible bit pattern maps to a valid [`SideCarKind`].
    #[inline]
    pub fn from_flags(flags: EdgeFlags) -> Self {
        let bits = (flags.bits() >> 2) & Self::MASK;

        match bits {
            0 => Self::None,
            1 => Self::QuantizedWeight,
            2 => Self::RankHint,
            3 => Self::RecencyBucket,
            _ => unreachable!(),
        }
    }

    /// Writes this sidecar kind into an existing [`EdgeFlags`] value.
    ///
    /// The previous sidecar-kind bits are cleared first. All unrelated flags,
    /// including reserved bits, are preserved.
    #[inline]
    pub fn apply(self, flags: &mut EdgeFlags) {
        // clear bits 2..3
        flags.remove(EdgeFlags::SIDECAR_KIND_0 | EdgeFlags::SIDECAR_KIND_1);

        let value = (self as u8) << 2;

        *flags |= EdgeFlags::from_bits_retain(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_packs_fields_into_documented_layout() {
        let mut flags = EdgeFlags::UNDIRECTED | EdgeFlags::REMOTE;
        SideCarKind::RankHint.apply(&mut flags);

        let meta = EdgeMeta::new(flags, 0xAB, LabelId::default());

        assert_eq!(meta.raw(), 0x0B_AB_00_00);
        assert_eq!(meta.to_le_bytes(), [0x00, 0x00, 0xAB, 0x0B]);
        assert_eq!(meta.flags(), flags);
        assert_eq!(meta.sidecar(), 0xAB);
        assert_eq!(meta.sidecar_kind(), SideCarKind::RankHint);
        assert_eq!(meta.label_id(), 0);
    }

    #[test]
    fn from_le_bytes_decodes_each_field_and_round_trips() {
        let meta = EdgeMeta::from_le_bytes([0x34, 0x12, 0x56, 0x8D]);

        assert_eq!(meta.label_id(), 0x1234);
        assert_eq!(meta.sidecar(), 0x56);
        assert_eq!(meta.flags().bits(), 0x8D);
        assert_eq!(meta.sidecar_kind(), SideCarKind::RecencyBucket);
        assert_eq!(meta.to_le_bytes(), [0x34, 0x12, 0x56, 0x8D]);
    }

    #[test]
    fn sidecar_kind_reads_only_kind_bits() {
        let cases = [
            (SideCarKind::None, 0b00),
            (SideCarKind::QuantizedWeight, 0b01),
            (SideCarKind::RankHint, 0b10),
            (SideCarKind::RecencyBucket, 0b11),
        ];

        for (kind, bits) in cases {
            let flags = EdgeFlags::from_bits_retain(0b1111_0011 | (bits << 2));

            assert_eq!(SideCarKind::from_flags(flags), kind);
        }
    }

    #[test]
    fn sidecar_kind_apply_replaces_kind_bits_and_preserves_other_flags() {
        let mut flags = EdgeFlags::UNDIRECTED
            | EdgeFlags::REMOTE
            | EdgeFlags::SIDECAR_KIND_0
            | EdgeFlags::RESERVED_4
            | EdgeFlags::RESERVED_7;

        SideCarKind::RecencyBucket.apply(&mut flags);

        assert!(flags.contains(EdgeFlags::UNDIRECTED));
        assert!(flags.contains(EdgeFlags::REMOTE));
        assert!(flags.contains(EdgeFlags::RESERVED_4));
        assert!(flags.contains(EdgeFlags::RESERVED_7));
        assert_eq!(SideCarKind::from_flags(flags), SideCarKind::RecencyBucket);
        assert_eq!(
            flags & (EdgeFlags::SIDECAR_KIND_0 | EdgeFlags::SIDECAR_KIND_1),
            EdgeFlags::SIDECAR_KIND_0 | EdgeFlags::SIDECAR_KIND_1
        );
    }

    #[test]
    fn flag_convenience_methods_toggle_only_their_flag() {
        let meta = EdgeMeta::from_le_bytes([0x34, 0x12, 0x56, 0xF0]);

        let undirected = meta.with_undirected(true);
        assert!(undirected.is_undirected());
        assert_eq!(undirected.raw(), 0xF1_56_12_34);

        let remote = undirected.with_remote(true);
        assert!(remote.is_remote());
        assert_eq!(remote.raw(), 0xF3_56_12_34);

        let directed_local = remote.with_undirected(false).with_remote(false);
        assert!(!directed_local.is_undirected());
        assert!(!directed_local.is_remote());
        assert_eq!(directed_local.raw(), meta.raw());
    }

    #[test]
    fn default_is_a_zeroed_metadata_word() {
        let meta = EdgeMeta::default();

        assert_eq!(meta.raw(), 0);
        assert_eq!(meta.to_le_bytes(), [0, 0, 0, 0]);
        assert_eq!(meta.flags(), EdgeFlags::empty());
        assert_eq!(meta.sidecar(), 0);
        assert_eq!(meta.sidecar_kind(), SideCarKind::None);
        assert_eq!(meta.label_id(), 0);
    }
}
