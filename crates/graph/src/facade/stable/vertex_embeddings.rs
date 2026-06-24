//! Canonical vertex embedding store (ADR 0031).
//!
//! Graph shards own canonical vertex embeddings. This store keys embeddings by
//! `(VertexId, EmbeddingNameId)` (vertex-major, big-endian, fixed-width) so a vertex delete can
//! enumerate every embedding it owns via a per-vertex range scan. Backfill-by-embedding-name is
//! deliberately not optimized here; a later derived `(EmbeddingNameId, VertexId)` access path may
//! be added when vector-index backfill needs it, but it must not become a second canonical store.

use gleaph_graph_kernel::entry::EmbeddingNameId;
use gleaph_graph_kernel::vector_index::VectorEncoding;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;
use std::ops::Bound as RangeBound;

/// Stored-record schema version. Bump only with a stable-memory migration.
const STORED_EMBEDDING_SCHEMA_V1: u8 = 1;

/// On-disk tag for [`VectorEncoding::F32`].
const ENCODING_TAG_F32: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VertexEmbeddingKey {
    vertex_id: u32,
    embedding_name_id: u16,
}

impl VertexEmbeddingKey {
    pub fn new(vertex_id: VertexId, embedding_name_id: EmbeddingNameId) -> Self {
        Self {
            vertex_id: u32::from_le_bytes(vertex_id.to_le_bytes()),
            embedding_name_id: embedding_name_id.raw(),
        }
    }

    pub fn vertex_id(self) -> VertexId {
        VertexId::from(self.vertex_id)
    }

    pub fn embedding_name_id(self) -> EmbeddingNameId {
        EmbeddingNameId::from_raw(self.embedding_name_id)
    }
}

impl Storable for VertexEmbeddingKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.vertex_id.to_be_bytes());
        out.extend_from_slice(&self.embedding_name_id.to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 6, "VertexEmbeddingKey expects exactly 6 bytes");
        let mut vertex = [0; 4];
        let mut name = [0; 2];
        vertex.copy_from_slice(&bytes[0..4]);
        name.copy_from_slice(&bytes[4..6]);
        Self {
            vertex_id: u32::from_be_bytes(vertex),
            embedding_name_id: u16::from_be_bytes(name),
        }
    }
}

/// Canonical embedding record for a `(VertexId, EmbeddingNameId)`.
///
/// V1 manual byte layout (length-prefixed, little-endian scalars):
/// `schema_version: u8 | encoding_tag: u8 | dims: u16 | version: u64 | len: u32 | bytes`.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEmbedding {
    pub encoding: VectorEncoding,
    pub dims: u16,
    pub version: u64,
    pub bytes: Vec<u8>,
}

impl Storable for StoredEmbedding {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let encoding_tag = match self.encoding {
            VectorEncoding::F32 => ENCODING_TAG_F32,
        };
        // The write boundary limits F32 bytes to `dims * 4` (dims: u16), so a V1 record's payload
        // is at most `u16::MAX * 4` bytes and always fits u32. Revisit this bound if a future
        // encoding allows wider payloads.
        let len: u32 = self
            .bytes
            .len()
            .try_into()
            .expect("embedding byte length must fit u32");
        let mut out = Vec::with_capacity(1 + 1 + 2 + 8 + 4 + self.bytes.len());
        out.push(STORED_EMBEDDING_SCHEMA_V1);
        out.push(encoding_tag);
        out.extend_from_slice(&self.dims.to_le_bytes());
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert!(
            bytes.len() >= 16,
            "StoredEmbedding record is truncated ({} bytes)",
            bytes.len()
        );
        // `Storable::from_bytes` cannot return an error, so an unknown schema version or encoding
        // tag is a hard trap: it can only happen on an incompatible stable layout that requires a
        // migration, not on normal reads.
        assert_eq!(
            bytes[0], STORED_EMBEDDING_SCHEMA_V1,
            "unknown StoredEmbedding schema version {} (stable-memory migration required)",
            bytes[0]
        );
        let encoding = match bytes[1] {
            ENCODING_TAG_F32 => VectorEncoding::F32,
            other => panic!("unknown StoredEmbedding encoding tag {other} (migration required)"),
        };
        let dims = u16::from_le_bytes([bytes[2], bytes[3]]);
        let mut version = [0; 8];
        version.copy_from_slice(&bytes[4..12]);
        let version = u64::from_le_bytes(version);
        let mut len = [0; 4];
        len.copy_from_slice(&bytes[12..16]);
        let len = u32::from_le_bytes(len) as usize;
        let payload = &bytes[16..];
        assert_eq!(
            payload.len(),
            len,
            "StoredEmbedding byte length mismatch (header {len}, actual {})",
            payload.len()
        );
        Self {
            encoding,
            dims,
            version,
            bytes: payload.to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexEmbeddingStoreError {
    /// `EmbeddingNameId(0)` is reserved and may not be written.
    ReservedEmbeddingName,
    /// Supplied byte buffer does not match `dims * 4` for the `F32` encoding.
    ByteWidthMismatch { expected: usize, actual: usize },
    /// An update supplied a different `dims` than the existing record.
    DimensionMismatch { existing: u16, requested: u16 },
    /// Version counter would overflow `u64`.
    VersionOverflow,
    /// Delete-spanning incarnation counter would overflow `u64` (ADR 0031 Slice 4). The incarnation
    /// is the vector-sync ordering fence; wrapping it to `0`/`1` would let a stale remove tombstone a
    /// live vector, so this is a hard checked failure rather than a silent wrap.
    IncarnationOverflow,
}

impl fmt::Display for VertexEmbeddingStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedEmbeddingName => write!(f, "embedding name id 0 is reserved"),
            Self::ByteWidthMismatch { expected, actual } => write!(
                f,
                "embedding byte width mismatch: expected {expected}, got {actual}"
            ),
            Self::DimensionMismatch {
                existing,
                requested,
            } => write!(
                f,
                "embedding dimension mismatch: existing {existing}, requested {requested} \
                 (dimension changes require remove + insert or a new embedding name)"
            ),
            Self::VersionOverflow => write!(f, "embedding version counter overflow"),
            Self::IncarnationOverflow => write!(f, "embedding incarnation counter overflow"),
        }
    }
}

impl std::error::Error for VertexEmbeddingStoreError {}

/// Result of a successful [`VertexEmbeddingStore::set`].
///
/// `incarnation` is the delete-spanning identity counter (ADR 0031 Slice 4): it strictly increases
/// across each delete/reinsert of a `(VertexId, EmbeddingNameId)` and is never reset, while
/// `version` is the per-incarnation update counter (resets to `1` on each fresh incarnation). The
/// vector canister orders writes by `(incarnation, version)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddingWrite {
    pub incarnation: u64,
    pub version: u64,
}

pub struct VertexEmbeddingStore<M: Memory> {
    embeddings: StableBTreeMap<VertexEmbeddingKey, StoredEmbedding, M>,
    /// Delete-spanning incarnation high-water mark per identity. Unlike `embeddings`, an entry here
    /// is **kept across `remove`** so a later reinsert allocates a strictly greater incarnation and
    /// a stale derived-index remove can never tombstone the newer live vector (ADR 0031 Slice 4).
    incarnations: StableBTreeMap<VertexEmbeddingKey, u64, M>,
}

impl<M: Memory> VertexEmbeddingStore<M> {
    pub fn init(embeddings: M, incarnations: M) -> Self {
        Self {
            embeddings: StableBTreeMap::init(embeddings),
            incarnations: StableBTreeMap::init(incarnations),
        }
    }

    pub fn get(
        &self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
    ) -> Option<StoredEmbedding> {
        if embedding_name_id.is_reserved() {
            return None;
        }
        self.embeddings
            .get(&VertexEmbeddingKey::new(vertex_id, embedding_name_id))
    }

    /// The delete-spanning incarnation for a `(VertexId, EmbeddingNameId)`, if it has ever been
    /// written. Survives `remove`, so the repair drain can stamp a canonical-absent remove with the
    /// last live incarnation (ADR 0031 Slice 4).
    pub fn incarnation_for(
        &self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
    ) -> Option<u64> {
        if embedding_name_id.is_reserved() {
            return None;
        }
        self.incarnations
            .get(&VertexEmbeddingKey::new(vertex_id, embedding_name_id))
    }

    /// Inserts or updates a vertex embedding.
    ///
    /// On a live record `version` is the previous version plus one and the incarnation is unchanged.
    /// When no live record exists (first insert, or reinsert after a remove) a fresh incarnation is
    /// allocated (`previous_incarnation + 1`, starting at `1`) and `version` resets to `1`.
    /// Dimension changes on an existing live record are rejected: changing dims requires remove +
    /// insert or a new embedding name.
    pub fn set(
        &mut self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
        encoding: VectorEncoding,
        dims: u16,
        bytes: Vec<u8>,
    ) -> Result<EmbeddingWrite, VertexEmbeddingStoreError> {
        if embedding_name_id.is_reserved() {
            return Err(VertexEmbeddingStoreError::ReservedEmbeddingName);
        }
        // Exhaustive match so a future F16/I8 variant forces an explicit byte-width branch here.
        let expected = match encoding {
            VectorEncoding::F32 => (dims as usize).saturating_mul(4),
        };
        if bytes.len() != expected {
            return Err(VertexEmbeddingStoreError::ByteWidthMismatch {
                expected,
                actual: bytes.len(),
            });
        }
        let key = VertexEmbeddingKey::new(vertex_id, embedding_name_id);
        let write = match self.embeddings.get(&key) {
            Some(existing) => {
                if existing.dims != dims {
                    return Err(VertexEmbeddingStoreError::DimensionMismatch {
                        existing: existing.dims,
                        requested: dims,
                    });
                }
                // Live record: keep the incarnation, bump the version.
                let incarnation = self.incarnations.get(&key).unwrap_or(1);
                let version = existing
                    .version
                    .checked_add(1)
                    .ok_or(VertexEmbeddingStoreError::VersionOverflow)?;
                EmbeddingWrite {
                    incarnation,
                    version,
                }
            }
            None => {
                // First insert or reinsert after remove: allocate a strictly greater incarnation
                // (the high-water mark persists across removes) and reset the version.
                let incarnation = match self.incarnations.get(&key) {
                    Some(previous) => previous
                        .checked_add(1)
                        .ok_or(VertexEmbeddingStoreError::IncarnationOverflow)?,
                    None => 1,
                };
                EmbeddingWrite {
                    incarnation,
                    version: 1,
                }
            }
        };
        self.incarnations.insert(key, write.incarnation);
        self.embeddings.insert(
            key,
            StoredEmbedding {
                encoding,
                dims,
                version: write.version,
                bytes,
            },
        );
        Ok(write)
    }

    /// Removes the live record, **keeping** the incarnation high-water mark. Returns the removed
    /// record paired with its incarnation so the caller can stamp an incarnation-fenced derived
    /// remove (ADR 0031 Slice 4).
    pub fn remove(
        &mut self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
    ) -> Option<(StoredEmbedding, u64)> {
        if embedding_name_id.is_reserved() {
            return None;
        }
        let key = VertexEmbeddingKey::new(vertex_id, embedding_name_id);
        let record = self.embeddings.remove(&key)?;
        // The incarnations entry is intentionally retained as a high-water mark. A live record
        // written before Slice 4 has no incarnation entry; treat it as the implicit first
        // incarnation (1), matching the `set` live branch.
        let incarnation = self.incarnations.get(&key).unwrap_or(1);
        Some((record, incarnation))
    }

    /// Embedding name ids owned by `vertex_id`, in key order.
    pub fn names_for(&self, vertex_id: VertexId) -> Vec<EmbeddingNameId> {
        let mut out = Vec::new();
        self.for_each_for(vertex_id, |name, _| out.push(name));
        out
    }

    /// Visits `(embedding_name_id, record)` for `vertex_id` in key order without an intermediate
    /// allocation.
    pub(crate) fn for_each_for<F>(&self, vertex_id: VertexId, mut f: F)
    where
        F: FnMut(EmbeddingNameId, StoredEmbedding),
    {
        let vertex_id_raw = u32::from_le_bytes(vertex_id.to_le_bytes());
        let start = VertexEmbeddingKey {
            vertex_id: vertex_id_raw,
            embedding_name_id: 0,
        };
        let upper = vertex_id_raw.checked_add(1).map(|next_vertex_id| {
            RangeBound::Excluded(VertexEmbeddingKey {
                vertex_id: next_vertex_id,
                embedding_name_id: 0,
            })
        });
        let range = match upper {
            Some(upper) => (RangeBound::Included(start), upper),
            None => (RangeBound::Included(start), RangeBound::Unbounded),
        };
        let vid = VertexId::from(vertex_id_raw);
        for entry in self
            .embeddings
            .range(range)
            .take_while(|entry| entry.key().vertex_id() == vid)
        {
            let (key, value) = entry.into_pair();
            f(key.embedding_name_id(), value);
        }
    }

    pub fn into_memories(self) -> (M, M) {
        (
            self.embeddings.into_memory(),
            self.incarnations.into_memory(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn store() -> VertexEmbeddingStore<VectorMemory> {
        VertexEmbeddingStore::init(VectorMemory::default(), VectorMemory::default())
    }

    fn vec_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn insert_sets_version_one_and_reads_back() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);
        let bytes = vec_bytes(&[1.0, 2.0, 3.0, 4.0]);

        assert_eq!(
            store
                .set(vid, name, VectorEncoding::F32, 4, bytes.clone())
                .unwrap(),
            EmbeddingWrite {
                incarnation: 1,
                version: 1,
            }
        );
        let record = store.get(vid, name).expect("record present");
        assert_eq!(record.dims, 4);
        assert_eq!(record.version, 1);
        assert_eq!(record.encoding, VectorEncoding::F32);
        assert_eq!(record.bytes, bytes);
        assert_eq!(store.incarnation_for(vid, name), Some(1));
    }

    #[test]
    fn update_bumps_version_not_incarnation_and_replaces_bytes() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);

        store
            .set(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
            .unwrap();
        let new_bytes = vec_bytes(&[9.0, 8.0]);
        assert_eq!(
            store
                .set(vid, name, VectorEncoding::F32, 2, new_bytes.clone())
                .unwrap(),
            EmbeddingWrite {
                incarnation: 1,
                version: 2,
            }
        );
        let record = store.get(vid, name).expect("record present");
        assert_eq!(record.version, 2);
        assert_eq!(record.bytes, new_bytes);
        assert_eq!(store.incarnation_for(vid, name), Some(1));
    }

    #[test]
    fn remove_keeps_incarnation_and_reinsert_bumps_it() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);

        store
            .set(vid, name, VectorEncoding::F32, 1, vec_bytes(&[1.0]))
            .unwrap();
        store
            .set(vid, name, VectorEncoding::F32, 1, vec_bytes(&[2.0]))
            .unwrap();

        let (record, incarnation) = store.remove(vid, name).expect("removed record");
        assert_eq!(record.version, 2);
        assert_eq!(incarnation, 1);
        // The live record is gone but the incarnation high-water mark survives the remove.
        assert!(store.get(vid, name).is_none());
        assert_eq!(store.incarnation_for(vid, name), Some(1));
        assert!(store.remove(vid, name).is_none());

        // Reinsert allocates a strictly greater incarnation and resets the version.
        assert_eq!(
            store
                .set(vid, name, VectorEncoding::F32, 1, vec_bytes(&[3.0]))
                .unwrap(),
            EmbeddingWrite {
                incarnation: 2,
                version: 1,
            }
        );
        assert_eq!(store.incarnation_for(vid, name), Some(2));
    }

    #[test]
    fn incarnation_for_is_none_until_first_write() {
        let store = store();
        assert_eq!(
            store.incarnation_for(VertexId::from(7), EmbeddingNameId::from_raw(1)),
            None
        );
    }

    #[test]
    fn rejects_incarnation_overflow_on_reinsert() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);

        // Seed the incarnation high-water mark at u64::MAX with no live record, so the next insert
        // would overflow the fence. Reaching u64::MAX through the public API is infeasible.
        store
            .incarnations
            .insert(VertexEmbeddingKey::new(vid, name), u64::MAX);

        let err = store
            .set(vid, name, VectorEncoding::F32, 1, vec_bytes(&[1.0]))
            .unwrap_err();
        assert_eq!(err, VertexEmbeddingStoreError::IncarnationOverflow);
        // The failed insert must not allocate a live record.
        assert!(store.get(vid, name).is_none());
        assert_eq!(store.incarnation_for(vid, name), Some(u64::MAX));
    }

    #[test]
    fn rejects_byte_width_mismatch() {
        let mut store = store();
        let err = store
            .set(
                VertexId::from(7),
                EmbeddingNameId::from_raw(1),
                VectorEncoding::F32,
                4,
                vec_bytes(&[1.0, 2.0]),
            )
            .unwrap_err();
        assert_eq!(
            err,
            VertexEmbeddingStoreError::ByteWidthMismatch {
                expected: 16,
                actual: 8,
            }
        );
    }

    #[test]
    fn rejects_dimension_change_on_update() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);

        store
            .set(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
            .unwrap();
        let err = store
            .set(
                vid,
                name,
                VectorEncoding::F32,
                3,
                vec_bytes(&[1.0, 2.0, 3.0]),
            )
            .unwrap_err();
        assert_eq!(
            err,
            VertexEmbeddingStoreError::DimensionMismatch {
                existing: 2,
                requested: 3,
            }
        );
    }

    #[test]
    fn rejects_version_overflow() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);

        // Seed a record already at the max version so the next update would overflow. Reaching
        // u64::MAX through the public API is infeasible, so insert directly via the private map.
        store.embeddings.insert(
            VertexEmbeddingKey::new(vid, name),
            StoredEmbedding {
                encoding: VectorEncoding::F32,
                dims: 1,
                version: u64::MAX,
                bytes: vec_bytes(&[1.0]),
            },
        );

        let err = store
            .set(vid, name, VectorEncoding::F32, 1, vec_bytes(&[2.0]))
            .unwrap_err();
        assert_eq!(err, VertexEmbeddingStoreError::VersionOverflow);
        // The failed update must not mutate the existing record.
        assert_eq!(
            store.get(vid, name).expect("record present").version,
            u64::MAX
        );
    }

    #[test]
    fn rejects_reserved_embedding_name() {
        let mut store = store();
        let err = store
            .set(
                VertexId::from(7),
                EmbeddingNameId::from_raw(0),
                VectorEncoding::F32,
                1,
                vec_bytes(&[1.0]),
            )
            .unwrap_err();
        assert_eq!(err, VertexEmbeddingStoreError::ReservedEmbeddingName);
        assert!(
            store
                .get(VertexId::from(7), EmbeddingNameId::from_raw(0))
                .is_none()
        );
    }

    #[test]
    fn names_for_returns_only_one_vertex() {
        let mut store = store();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);
        let one = EmbeddingNameId::from_raw(1);
        let two = EmbeddingNameId::from_raw(2);

        store
            .set(alice, two, VectorEncoding::F32, 1, vec_bytes(&[2.0]))
            .unwrap();
        store
            .set(alice, one, VectorEncoding::F32, 1, vec_bytes(&[1.0]))
            .unwrap();
        store
            .set(bob, one, VectorEncoding::F32, 1, vec_bytes(&[3.0]))
            .unwrap();

        assert_eq!(store.names_for(alice), vec![one, two]);
        assert_eq!(store.names_for(bob), vec![one]);
    }

    #[test]
    fn names_for_handles_max_vertex_id() {
        let mut store = store();
        let max = VertexId::from(u32::MAX);
        let name = EmbeddingNameId::from_raw(1);

        store
            .set(max, name, VectorEncoding::F32, 1, vec_bytes(&[1.0]))
            .unwrap();
        assert_eq!(store.names_for(max), vec![name]);
    }

    #[test]
    fn persists_across_reopen() {
        let mut store = store();
        let vid = VertexId::from(7);
        let name = EmbeddingNameId::from_raw(1);
        let bytes = vec_bytes(&[1.0, 2.0, 3.0, 4.0]);

        store
            .set(vid, name, VectorEncoding::F32, 4, bytes.clone())
            .unwrap();
        let (embeddings_mem, incarnations_mem) = store.into_memories();

        let reopened = VertexEmbeddingStore::init(embeddings_mem, incarnations_mem);
        let record = reopened.get(vid, name).expect("record present");
        assert_eq!(record.version, 1);
        assert_eq!(record.dims, 4);
        assert_eq!(record.bytes, bytes);
        assert_eq!(reopened.incarnation_for(vid, name), Some(1));
    }
}
