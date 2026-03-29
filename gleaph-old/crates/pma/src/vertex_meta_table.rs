//! Stable-memory `VertexMetaTable`: a B+ tree keyed by `vertex_id` storing per-vertex labels.
//!
//! Each entry maps a `vertex_id: u32` to its set of labels (UTF-8 strings).
//! The table is backed by an [`AbpByteKv`] instance so reads/writes are O(log n).
//! Keys are 4-byte big-endian `vertex_id` values for lexicographic order.
//!
//! # Value layout (variable length)
//!
//! ```text
//! offset  len   field
//! ──────  ───   ─────────────────────────────────────────────────────────
//!      0    2   num_labels: u16 LE
//!      2    *   labels[0]: [len: u16 LE] [utf8 bytes]
//!             labels[1]: [len: u16 LE] [utf8 bytes]
//!             ...
//! ```
//!
//! Total minimum value size: 2 bytes (0 labels).

use crate::{
    abp_tree::AbpByteKv,
    memory::{Memory, MemoryError},
};

/// Minimum initial region size: ABP store header + one page.
pub const VERTEX_META_MIN_REGION: u64 = {
    use crate::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN};
    ABP_STORE_HEADER_LEN + ABP_PAGE_SIZE as u64
};

// ── VertexMeta ──────────────────────────────────────────────────────────────────

/// Per-vertex metadata stored in [`VertexMetaTable`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexMeta {
    /// Labels assigned to this vertex.
    pub labels: Vec<String>,
}

impl VertexMeta {
    /// Encodes the metadata into a variable-length byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let num = self.labels.len() as u16;
        out.extend_from_slice(&num.to_le_bytes());
        for label in &self.labels {
            let bytes = label.as_bytes();
            let len = bytes.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        out
    }

    /// Decodes a metadata value from bytes. Returns `None` if the slice is malformed.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 2 {
            return None;
        }
        let num = u16::from_le_bytes(bytes[0..2].try_into().ok()?) as usize;
        let mut offset = 2;
        let mut labels = Vec::with_capacity(num);
        for _ in 0..num {
            if offset + 2 > bytes.len() {
                return None;
            }
            let len = u16::from_le_bytes(bytes[offset..offset + 2].try_into().ok()?) as usize;
            offset += 2;
            if offset + len > bytes.len() {
                return None;
            }
            let s = std::str::from_utf8(&bytes[offset..offset + len]).ok()?;
            labels.push(s.to_owned());
            offset += len;
        }
        Some(Self { labels })
    }

    /// Encodes a `vertex_id` as a 4-byte big-endian key.
    pub fn vertex_key(vertex_id: u32) -> [u8; 4] {
        vertex_id.to_be_bytes()
    }
}

// ── VertexMetaTable ─────────────────────────────────────────────────────────────

/// Wraps an [`AbpByteKv`] to provide typed `VertexMeta` read/write operations.
pub struct VertexMetaTable<M: Memory> {
    kv: AbpByteKv<M>,
}

impl<M: Memory> VertexMetaTable<M> {
    /// Creates a new, empty vertex-meta table at `region_start` in `mem`.
    pub fn create(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self {
            kv: AbpByteKv::create(mem, region_start)?,
        })
    }

    /// Opens an existing vertex-meta table from `mem` at `region_start`.
    pub fn open(mem: M, region_start: u64) -> Result<Self, MemoryError> {
        Ok(Self {
            kv: AbpByteKv::open(mem, region_start)?,
        })
    }

    /// Consumes the table, returning the underlying memory.
    pub fn into_memory(self) -> M {
        self.kv.into_memory()
    }

    /// Returns a reference to the underlying memory.
    pub fn memory(&self) -> &M {
        self.kv.memory()
    }

    /// Returns a mutable reference to the underlying memory.
    pub fn memory_mut(&mut self) -> &mut M {
        self.kv.memory_mut()
    }

    /// Looks up the labels for `vertex_id`.
    pub fn get_vertex_meta(&self, vertex_id: u32) -> Option<VertexMeta> {
        let key = VertexMeta::vertex_key(vertex_id);
        let value = self.kv.get(&key)?;
        VertexMeta::decode(&value)
    }

    /// Inserts or replaces the labels for `vertex_id`.
    pub fn set_vertex_meta(
        &mut self,
        vertex_id: u32,
        meta: &VertexMeta,
    ) -> Result<(), MemoryError> {
        let key = VertexMeta::vertex_key(vertex_id);
        let value = meta.encode();
        self.kv.upsert(&key, &value)
    }

    /// Removes the entry for `vertex_id`.
    pub fn delete_vertex_meta(&mut self, vertex_id: u32) -> Result<(), MemoryError> {
        let key = VertexMeta::vertex_key(vertex_id);
        self.kv.delete(&key)
    }

    /// Returns all `(vertex_id, VertexMeta)` entries in key-sorted order.
    pub fn iter_all(&self) -> Vec<(u32, VertexMeta)> {
        self.kv
            .scan_prefix(&[])
            .into_iter()
            .filter_map(|(key, value)| {
                if key.len() < 4 {
                    return None;
                }
                let vertex_id = u32::from_be_bytes(key[..4].try_into().ok()?);
                let meta = VertexMeta::decode(&value)?;
                Some((vertex_id, meta))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VecMemory;

    fn create_table() -> VertexMetaTable<VecMemory> {
        let mut mem = VecMemory::default();
        mem.grow(VERTEX_META_MIN_REGION + 65536).unwrap();
        VertexMetaTable::create(mem, 0).unwrap()
    }

    #[test]
    fn vertex_meta_encode_decode_round_trip() {
        let meta = VertexMeta {
            labels: vec!["Person".to_string(), "Employee".to_string()],
        };
        let encoded = meta.encode();
        let decoded = VertexMeta::decode(&encoded).expect("decode");
        assert_eq!(decoded, meta);

        // Empty labels.
        let empty = VertexMeta { labels: vec![] };
        let enc = empty.encode();
        let dec = VertexMeta::decode(&enc).expect("decode empty");
        assert_eq!(dec.labels.len(), 0);
    }

    #[test]
    fn vertex_meta_table_crud() {
        let mut tbl = create_table();

        // Insert.
        let meta1 = VertexMeta {
            labels: vec!["User".into(), "Admin".into()],
        };
        tbl.set_vertex_meta(5, &meta1).expect("set");

        // Get.
        let got = tbl.get_vertex_meta(5).expect("get");
        assert_eq!(got, meta1);

        // Update.
        let meta2 = VertexMeta {
            labels: vec!["User".into()],
        };
        tbl.set_vertex_meta(5, &meta2).expect("update");
        let got2 = tbl.get_vertex_meta(5).expect("get updated");
        assert_eq!(got2, meta2);

        // Delete.
        tbl.delete_vertex_meta(5).expect("delete");
        assert!(tbl.get_vertex_meta(5).is_none());

        // Missing key returns None.
        assert!(tbl.get_vertex_meta(99).is_none());
    }

    #[test]
    fn vertex_meta_table_iter_all() {
        let mut tbl = create_table();

        tbl.set_vertex_meta(
            0,
            &VertexMeta {
                labels: vec!["A".into()],
            },
        )
        .unwrap();
        tbl.set_vertex_meta(
            10,
            &VertexMeta {
                labels: vec!["B".into(), "C".into()],
            },
        )
        .unwrap();
        tbl.set_vertex_meta(
            5,
            &VertexMeta {
                labels: vec!["D".into()],
            },
        )
        .unwrap();

        let entries = tbl.iter_all();
        assert_eq!(entries.len(), 3);
        // Should be ordered by vertex_id (big-endian key ordering).
        assert_eq!(entries[0].0, 0);
        assert_eq!(entries[1].0, 5);
        assert_eq!(entries[2].0, 10);
    }

    #[test]
    fn vertex_meta_table_reopen() {
        let mut tbl = create_table();
        tbl.set_vertex_meta(
            42,
            &VertexMeta {
                labels: vec!["Foo".into()],
            },
        )
        .unwrap();

        let mem = tbl.into_memory();
        let tbl2 = VertexMetaTable::open(mem, 0).unwrap();
        let got = tbl2.get_vertex_meta(42).expect("after reopen");
        assert_eq!(got.labels, vec!["Foo"]);
    }
}
