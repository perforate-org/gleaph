//! Fixed-width touched-subject key for one physical property-index build.

use std::borrow::Cow;

use gleaph_graph_kernel::index::{IndexBuildSubject, PhysicalIndexId};
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;

const TOUCHED_KEY_MAGIC: u8 = 1;
const TOUCHED_KEY_BYTES: usize = 24;
const VERTEX_SUBJECT_TAG: u8 = 0;
const EDGE_SUBJECT_TAG: u8 = 1;

/// Stable touched-set key. `PhysicalIndexId` is the only generation/namespace component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IndexBuildTouchedKey {
    pub(crate) physical_index_id: PhysicalIndexId,
    pub(crate) subject: IndexBuildSubject,
}

impl IndexBuildTouchedKey {
    pub(crate) const fn new(
        physical_index_id: PhysicalIndexId,
        subject: IndexBuildSubject,
    ) -> Self {
        Self {
            physical_index_id,
            subject,
        }
    }

    pub(crate) const fn prefix_lower(physical_index_id: PhysicalIndexId) -> Self {
        Self::new(
            physical_index_id,
            IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: 0,
            },
        )
    }

    fn encode(self) -> [u8; TOUCHED_KEY_BYTES] {
        let mut out = [0; TOUCHED_KEY_BYTES];
        out[0] = TOUCHED_KEY_MAGIC;
        out[1..9].copy_from_slice(&self.physical_index_id.to_le_bytes());
        match self.subject {
            IndexBuildSubject::Vertex {
                shard_id,
                vertex_id,
            } => {
                out[9] = VERTEX_SUBJECT_TAG;
                out[10..14].copy_from_slice(&shard_id.to_le_bytes());
                out[14..18].copy_from_slice(&vertex_id.to_le_bytes());
            }
            IndexBuildSubject::Edge {
                shard_id,
                owner_vertex_id,
                label_id,
                slot_index,
            } => {
                out[9] = EDGE_SUBJECT_TAG;
                out[10..14].copy_from_slice(&shard_id.to_le_bytes());
                out[14..18].copy_from_slice(&owner_vertex_id.to_le_bytes());
                out[18..20].copy_from_slice(&label_id.to_le_bytes());
                out[20..24].copy_from_slice(&slot_index.to_le_bytes());
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), TOUCHED_KEY_BYTES, "invalid touched key width");
        assert_eq!(bytes[0], TOUCHED_KEY_MAGIC, "invalid touched key format");

        let physical_index_id = PhysicalIndexId::from_le_bytes(
            bytes[1..9]
                .try_into()
                .expect("physical index id slice has fixed width"),
        )
        .expect("physical index id zero is reserved");
        let shard_id = u32::from_le_bytes(
            bytes[10..14]
                .try_into()
                .expect("shard id slice has fixed width"),
        );
        let primary_id = u32::from_le_bytes(
            bytes[14..18]
                .try_into()
                .expect("subject id slice has fixed width"),
        );
        let subject = match bytes[9] {
            VERTEX_SUBJECT_TAG => {
                assert_eq!(
                    &bytes[18..24],
                    &[0; 6],
                    "vertex touched key reserved bytes must be zero"
                );
                IndexBuildSubject::Vertex {
                    shard_id,
                    vertex_id: primary_id,
                }
            }
            EDGE_SUBJECT_TAG => IndexBuildSubject::Edge {
                shard_id,
                owner_vertex_id: primary_id,
                label_id: u16::from_le_bytes(
                    bytes[18..20]
                        .try_into()
                        .expect("label id slice has fixed width"),
                ),
                slot_index: u32::from_le_bytes(
                    bytes[20..24]
                        .try_into()
                        .expect("slot index slice has fixed width"),
                ),
            },
            _ => panic!("invalid touched subject tag"),
        };
        Self::new(physical_index_id, subject)
    }
}

impl Storable for IndexBuildTouchedKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: TOUCHED_KEY_BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touched_subject_keys_are_fixed_width_and_round_trip() {
        let physical_index_id = PhysicalIndexId::new(17).expect("non-zero physical id");
        for subject in [
            IndexBuildSubject::Vertex {
                shard_id: 2,
                vertex_id: 91,
            },
            IndexBuildSubject::Edge {
                shard_id: 3,
                owner_vertex_id: 92,
                label_id: 7,
                slot_index: 11,
            },
        ] {
            let key = IndexBuildTouchedKey::new(physical_index_id, subject);
            let bytes = Storable::into_bytes(key);
            assert_eq!(bytes.len(), TOUCHED_KEY_BYTES);
            assert_eq!(IndexBuildTouchedKey::from_bytes(Cow::Owned(bytes)), key);
        }
    }
}
