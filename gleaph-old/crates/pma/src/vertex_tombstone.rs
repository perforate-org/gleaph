//! Stable-memory bitset for vertex tombstones.
//!
//! One bit per vertex ID. Bit `i` is set iff vertex `i` has been tombstoned (soft-deleted).
//! The bitset occupies a contiguous region in stable memory: byte `region_start + i / 8`,
//! bit `i % 8`.
//!
//! For 1M vertices the region is only 128 KiB — far cheaper than serialising a
//! `BTreeSet<u32>` in the overlay CBOR blob.

use gleaph_types::VertexIdSet;

use crate::memory::Memory;

/// A stable-memory bitset tracking tombstoned (soft-deleted) vertices.
pub struct VertexTombstoneBitset<M: Memory> {
    mem: M,
    region_start: u64,
    num_bytes: u64,
}

impl<M: Memory> VertexTombstoneBitset<M> {
    /// Creates a new (zeroed) bitset region for `num_vertices` vertices.
    ///
    /// The caller must have already grown memory so that the region
    /// `[region_start, region_start + bytes_needed(num_vertices))` is valid.
    pub fn create(mem: M, region_start: u64, num_vertices: u32) -> Self {
        let num_bytes = Self::bytes_needed(num_vertices);
        let mut bs = Self {
            mem,
            region_start,
            num_bytes,
        };
        // Zero the region.
        let zeros = vec![0u8; num_bytes as usize];
        bs.mem.write(region_start, &zeros);
        bs
    }

    /// Opens an existing bitset region.
    pub fn open(mem: M, region_start: u64, num_vertices: u32) -> Self {
        let num_bytes = Self::bytes_needed(num_vertices);
        Self {
            mem,
            region_start,
            num_bytes,
        }
    }

    /// Returns the number of bytes needed for `num_vertices` vertices.
    pub fn bytes_needed(num_vertices: u32) -> u64 {
        (num_vertices as u64).div_ceil(8)
    }

    /// Returns `true` if the given vertex ID is tombstoned.
    pub fn is_tombstoned(&self, vertex_id: u32) -> bool {
        let byte_offset = self.region_start + (vertex_id as u64) / 8;
        let bit = (vertex_id % 8) as u8;
        let mut buf = [0u8; 1];
        self.mem.read(byte_offset, &mut buf);
        (buf[0] >> bit) & 1 == 1
    }

    /// Sets or clears the tombstone bit for a vertex.
    pub fn set_tombstoned(&mut self, vertex_id: u32, tombstoned: bool) {
        let byte_offset = self.region_start + (vertex_id as u64) / 8;
        let bit = (vertex_id % 8) as u8;
        let mut buf = [0u8; 1];
        self.mem.read(byte_offset, &mut buf);
        if tombstoned {
            buf[0] |= 1 << bit;
        } else {
            buf[0] &= !(1 << bit);
        }
        self.mem.write(byte_offset, &buf);
    }

    /// Bulk-writes the bitset from a `BTreeSet` of tombstoned vertex IDs.
    ///
    /// This first zeroes the entire region, then sets bits for each ID in the set.
    pub fn bulk_write_from_set(&mut self, set: &VertexIdSet) {
        // Zero the region first.
        let zeros = vec![0u8; self.num_bytes as usize];
        self.mem.write(self.region_start, &zeros);

        // Build a byte buffer and set bits, then write in one go.
        let mut buf = vec![0u8; self.num_bytes as usize];
        for v in set {
            let byte_idx = (v / 8) as usize;
            let bit = (v % 8) as u8;
            if byte_idx < buf.len() {
                buf[byte_idx] |= 1 << bit;
            }
        }
        self.mem.write(self.region_start, &buf);
    }

    /// Collects all tombstoned vertex IDs into a `VertexIdSet`.
    pub fn collect_all(&self) -> VertexIdSet {
        let mut set = VertexIdSet::new();
        let mut buf = vec![0u8; self.num_bytes as usize];
        self.mem.read(self.region_start, &mut buf);
        for (byte_idx, &byte) in buf.iter().enumerate() {
            if byte == 0 {
                continue;
            }
            for bit in 0..8u8 {
                if (byte >> bit) & 1 == 1 {
                    set.insert((byte_idx as u32) * 8 + bit as u32);
                }
            }
        }
        set
    }

    /// Consumes the bitset, returning the underlying memory.
    pub fn into_memory(self) -> M {
        self.mem
    }

    /// Returns a reference to the underlying memory.
    pub fn memory(&self) -> &M {
        &self.mem
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VecMemory;

    #[test]
    fn vertex_tombstone_bitset_round_trip() {
        let mut mem = VecMemory::default();
        let region_start = 4096u64;
        let num_vertices = 100u32;
        let needed = VertexTombstoneBitset::<VecMemory>::bytes_needed(num_vertices);
        mem.grow(region_start + needed).unwrap();

        let mut bs = VertexTombstoneBitset::create(mem, region_start, num_vertices);

        // Initially nothing is tombstoned.
        assert!(!bs.is_tombstoned(0));
        assert!(!bs.is_tombstoned(50));
        assert!(!bs.is_tombstoned(99));

        // Tombstone some vertices.
        bs.set_tombstoned(0, true);
        bs.set_tombstoned(7, true);
        bs.set_tombstoned(8, true);
        bs.set_tombstoned(99, true);

        assert!(bs.is_tombstoned(0));
        assert!(bs.is_tombstoned(7));
        assert!(bs.is_tombstoned(8));
        assert!(bs.is_tombstoned(99));
        assert!(!bs.is_tombstoned(1));
        assert!(!bs.is_tombstoned(50));

        // Clear a tombstone.
        bs.set_tombstoned(7, false);
        assert!(!bs.is_tombstoned(7));

        // Reopen and verify persistence.
        let mem2 = bs.into_memory();
        let bs2 = VertexTombstoneBitset::open(mem2, region_start, num_vertices);
        assert!(bs2.is_tombstoned(0));
        assert!(!bs2.is_tombstoned(7));
        assert!(bs2.is_tombstoned(8));
        assert!(bs2.is_tombstoned(99));
    }

    #[test]
    fn bulk_write_from_set_matches_individual_writes() {
        let mut mem = VecMemory::default();
        let region_start = 0u64;
        let num_vertices = 256u32;
        let needed = VertexTombstoneBitset::<VecMemory>::bytes_needed(num_vertices);
        mem.grow(needed).unwrap();

        let set = VertexIdSet::from_iter([0, 3, 17, 64, 255]);

        let mut bs = VertexTombstoneBitset::create(mem, region_start, num_vertices);
        bs.bulk_write_from_set(&set);

        let collected = bs.collect_all();
        assert_eq!(collected, set);

        // Verify individual reads match.
        for v in 0..num_vertices {
            assert_eq!(bs.is_tombstoned(v), set.contains(v), "mismatch at {v}");
        }
    }
}
