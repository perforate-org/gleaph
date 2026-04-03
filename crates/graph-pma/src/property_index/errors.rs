use std::fmt;

use crate::low_level::RegionKind;

use super::PropertyIndexNodeId;

/// Why [`PropertyIndexNodeStore::incremental_leaf_chain_shape`] (or
/// [`PropertyIndexNodeStore::try_incremental_leaf_chain_shape`]) could not build a consistent
/// `(ordered leaf ids, internal ids, fanout)` view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyIndexLeafChainShapeError {
    LeafOnlyStoreContainsInternalNode,
    CannotInferFirstLeafInLeafOnlyStore,
    NextLeafCycle { at: PropertyIndexNodeId },
    NextLeafNotLeaf { at: PropertyIndexNodeId },
    NextLeafChainLenMismatch { visited: usize, expected: usize },
    InternalRootMissing,
    InternalLeftmostLeafUnreachable { root: PropertyIndexNodeId },
}

impl fmt::Display for PropertyIndexLeafChainShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LeafOnlyStoreContainsInternalNode => write!(
                f,
                "leaf-only incremental shape path saw an internal node record"
            ),
            Self::CannotInferFirstLeafInLeafOnlyStore => {
                write!(f, "could not infer first leaf in a leaf-only node store")
            }
            Self::NextLeafCycle { at } => {
                write!(f, "next_leaf walk cycle at node id {}", at.0)
            }
            Self::NextLeafNotLeaf { at } => write!(
                f,
                "next_leaf pointed at non-leaf or missing node id {}",
                at.0
            ),
            Self::NextLeafChainLenMismatch { visited, expected } => write!(
                f,
                "next_leaf chain visited {visited} leaves but store has {expected} leaf records"
            ),
            Self::InternalRootMissing => write!(f, "could not infer internal B-tree root"),
            Self::InternalLeftmostLeafUnreachable { root } => write!(
                f,
                "leftmost leaf under internal root {} is unreachable (cycle or invalid child link)",
                root.0
            ),
        }
    }
}

impl std::error::Error for PropertyIndexLeafChainShapeError {}

/// Error type for rewrite-side property-index skeletons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyIndexError {
    InvalidRegionHeaderLength(usize),
    InvalidHeaderLength(usize),
    InvalidAllocatorHeaderLength(usize),
    InvalidNodeHeaderLength(usize),
    InvalidKeyLength(usize),
    KeyLengthMismatch {
        expected: usize,
        actual: usize,
    },
    RecordTooShort(usize),
    RecordLengthMismatch {
        expected: usize,
        actual: usize,
    },
    UnknownEntityKind(u8),
    UnknownNodeKind(u8),
    InvalidMagic(Vec<u8>),
    UnsupportedVersion(u8),
    InvalidNodePageLength(usize),
    InvalidNodePageMagic(Vec<u8>),
    UnsupportedNodePageVersion(u8),
    InvalidPagedAreaMagic(Vec<u8>),
    UnsupportedPagedAreaVersion(u8),
    InvalidOverflowPageMagic(Vec<u8>),
    UnsupportedOverflowPageVersion(u8),
    MissingNodeSlot(PropertyIndexNodeId),
    OverflowPersistenceNotYetSupported(PropertyIndexNodeId),
    MissingOverflowPage(usize),
    TruncatedNodeOverflowChain {
        expected_payload_len: usize,
        decoded_payload_len: usize,
    },
    NodePageTooSmall(usize),
    NullNodeId,
    NodeTooLarge {
        encoded_len: usize,
        page_size: usize,
    },
    InvalidUtf8(std::str::Utf8Error),
    LengthOverflow,
    MissingPropertyIndexRegion(RegionKind),
    RegionTooSmall {
        kind: RegionKind,
        required: u64,
        capacity: u64,
    },
    TruncatedBucketChain {
        kind: RegionKind,
        logical_len: usize,
        read: usize,
    },
    LeafPartitionMultiEntryExceedsPrimaryPage,
    LeafPartitionSingletonNotEncodable,
}

impl fmt::Display for PropertyIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegionHeaderLength(len) => {
                write!(f, "invalid property-index region header length: {len}")
            }
            Self::InvalidHeaderLength(len) => {
                write!(f, "invalid property-index header length: {len}")
            }
            Self::InvalidAllocatorHeaderLength(len) => {
                write!(f, "invalid property-index allocator header length: {len}")
            }
            Self::InvalidNodeHeaderLength(len) => {
                write!(f, "invalid property-index node header length: {len}")
            }
            Self::InvalidKeyLength(len) => write!(f, "invalid property-index key length: {len}"),
            Self::KeyLengthMismatch { expected, actual } => write!(
                f,
                "property-index key length mismatch: expected {expected}, got {actual}"
            ),
            Self::RecordTooShort(len) => write!(f, "property-index record too short: {len}"),
            Self::RecordLengthMismatch { expected, actual } => write!(
                f,
                "property-index record length mismatch: expected {expected}, got {actual}"
            ),
            Self::UnknownEntityKind(tag) => {
                write!(f, "unknown property-index entity kind tag: {tag}")
            }
            Self::UnknownNodeKind(tag) => write!(f, "unknown property-index node kind tag: {tag}"),
            Self::InvalidMagic(magic) => write!(f, "invalid property-index magic: {magic:?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported property-index snapshot version: {version}")
            }
            Self::InvalidNodePageLength(len) => {
                write!(f, "invalid property-index node page length: {len}")
            }
            Self::InvalidNodePageMagic(magic) => {
                write!(f, "invalid property-index node page magic: {magic:?}")
            }
            Self::UnsupportedNodePageVersion(version) => {
                write!(f, "unsupported property-index node page version: {version}")
            }
            Self::InvalidPagedAreaMagic(magic) => {
                write!(f, "invalid property-index paged area magic: {magic:?}")
            }
            Self::UnsupportedPagedAreaVersion(version) => {
                write!(
                    f,
                    "unsupported property-index paged area version: {version}"
                )
            }
            Self::InvalidOverflowPageMagic(magic) => {
                write!(f, "invalid property-index overflow page magic: {magic:?}")
            }
            Self::UnsupportedOverflowPageVersion(version) => {
                write!(
                    f,
                    "unsupported property-index overflow page version: {version}"
                )
            }
            Self::MissingNodeSlot(node_id) => {
                write!(
                    f,
                    "missing property-index node slot for node id {}",
                    node_id.0
                )
            }
            Self::OverflowPersistenceNotYetSupported(node_id) => write!(
                f,
                "property-index node {} requires overflow-page persistence that is not yet stored in paged areas",
                node_id.0
            ),
            Self::MissingOverflowPage(page_index) => {
                write!(
                    f,
                    "missing property-index overflow page at index {page_index}"
                )
            }
            Self::TruncatedNodeOverflowChain {
                expected_payload_len,
                decoded_payload_len,
            } => write!(
                f,
                "truncated property-index node overflow chain: expected {expected_payload_len} payload bytes, decoded {decoded_payload_len}"
            ),
            Self::NodePageTooSmall(page_size) => {
                write!(
                    f,
                    "property-index node page size is too small: {page_size} bytes"
                )
            }
            Self::NullNodeId => write!(f, "null property-index node id has no page slot"),
            Self::NodeTooLarge {
                encoded_len,
                page_size,
            } => write!(
                f,
                "property-index node record is too large for one page: encoded {encoded_len} bytes, page size {page_size} bytes"
            ),
            Self::InvalidUtf8(err) => write!(f, "invalid UTF-8 in property-index key: {err}"),
            Self::LengthOverflow => write!(f, "property-index length overflow"),
            Self::MissingPropertyIndexRegion(kind) => {
                write!(f, "missing property-index region: {kind:?}")
            }
            Self::RegionTooSmall {
                kind,
                required,
                capacity,
            } => write!(
                f,
                "property-index region too small for {kind:?}: required {required} bytes, capacity {capacity} bytes"
            ),
            Self::TruncatedBucketChain {
                kind,
                logical_len,
                read,
            } => write!(
                f,
                "property-index bucket chain truncated for {kind:?}: logical length {logical_len} bytes, read only {read} bytes"
            ),
            Self::LeafPartitionMultiEntryExceedsPrimaryPage => write!(
                f,
                "property-index leaf partition: multi-entry chunk exceeds one primary page"
            ),
            Self::LeafPartitionSingletonNotEncodable => write!(
                f,
                "property-index leaf partition: singleton chunk is not encodable for paged storage"
            ),
        }
    }
}

impl std::error::Error for PropertyIndexError {}
