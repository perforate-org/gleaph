use crate::memory::Memory;
use gleaph_types::{EdgeEntry, LogEntry, StableHeader, VertexEntry};

pub const HEADER_BASE: u64 = 0x0;
pub const HEADER_SIZE: u64 = 4096;
pub const VERTEX_ARRAY_BASE: u64 = 0x1000;
pub const VERTEX_ENTRY_SIZE: u64 = core::mem::size_of::<VertexEntry>() as u64;
pub const EDGE_ENTRY_SIZE: u64 = core::mem::size_of::<EdgeEntry>() as u64;
/// Stable-memory serialized size of a LogEntry.
pub const LOG_ENTRY_SIZE: u64 = 32;
pub const SEG_TREE_ENTRY_SIZE: u64 = 8;
pub const MAX_LOG_ENTRIES_PER_SEGMENT: u64 = 145;
pub const SEG_LOG_INDEX_SIZE: u64 = 4;

/// Computes the base offset of the PMA edge array region.
pub fn edge_array_base(num_vertices: u64) -> u64 {
    VERTEX_ARRAY_BASE + num_vertices * VERTEX_ENTRY_SIZE
}

/// Computes the base offset of the segment tree metadata region.
pub fn seg_tree_base(num_vertices: u64, elem_capacity: u64) -> u64 {
    edge_array_base(num_vertices) + elem_capacity * EDGE_ENTRY_SIZE
}

/// Returns the byte size of the segment-tree "actual count" region.
pub fn seg_tree_actual_region_size(segment_count: u64) -> u64 {
    segment_count * SEG_TREE_ENTRY_SIZE
}

/// Returns the byte size of the segment-tree "total capacity" region.
pub fn seg_tree_total_region_size(segment_count: u64) -> u64 {
    segment_count * SEG_TREE_ENTRY_SIZE
}

/// Computes the base offset of the per-segment overflow log region.
pub fn seg_log_base(num_vertices: u64, elem_capacity: u64, segment_count: u64) -> u64 {
    seg_tree_base(num_vertices, elem_capacity)
        + seg_tree_actual_region_size(segment_count)
        + seg_tree_total_region_size(segment_count)
}

/// Computes the base offset of the per-segment log fill-index region.
pub fn seg_log_idx_base(num_vertices: u64, elem_capacity: u64, segment_count: u64) -> u64 {
    seg_log_base(num_vertices, elem_capacity, segment_count)
        + segment_count * MAX_LOG_ENTRIES_PER_SEGMENT * LOG_ENTRY_SIZE
}

/// Computes the total stable-memory footprint for the PMA layout.
pub fn total_memory_needed(num_vertices: u64, elem_capacity: u64, segment_count: u64) -> u64 {
    seg_log_idx_base(num_vertices, elem_capacity, segment_count)
        + segment_count * SEG_LOG_INDEX_SIZE
}

fn read_u32<M: Memory>(mem: &M, offset: u64) -> u32 {
    let mut buf = [0u8; 4];
    mem.read(offset, &mut buf);
    u32::from_le_bytes(buf)
}

fn write_u32<M: Memory>(mem: &mut M, offset: u64, value: u32) {
    mem.write(offset, &value.to_le_bytes());
}

fn read_u64<M: Memory>(mem: &M, offset: u64) -> u64 {
    let mut buf = [0u8; 8];
    mem.read(offset, &mut buf);
    u64::from_le_bytes(buf)
}

fn write_u64<M: Memory>(mem: &mut M, offset: u64, value: u64) {
    mem.write(offset, &value.to_le_bytes());
}

/// Reads a vertex entry by vertex identifier from stable memory.
pub fn read_vertex<M: Memory>(mem: &M, vertex_id: u32) -> VertexEntry {
    let off = VERTEX_ARRAY_BASE + vertex_id as u64 * VERTEX_ENTRY_SIZE;
    let edge_index = read_u64(mem, off);
    let degree = read_u32(mem, off + 8);
    let log_offset = read_u32(mem, off + 12) as i32;
    VertexEntry {
        edge_index,
        degree,
        log_offset,
    }
}

/// Writes a vertex entry by vertex identifier into stable memory.
pub fn write_vertex<M: Memory>(mem: &mut M, vertex_id: u32, v: &VertexEntry) {
    let off = VERTEX_ARRAY_BASE + vertex_id as u64 * VERTEX_ENTRY_SIZE;
    write_u64(mem, off, v.edge_index);
    write_u32(mem, off + 8, v.degree);
    write_u32(mem, off + 12, v.log_offset as u32);
}

/// Reads `count` contiguous edge entries starting at `start_slot` in a single bulk memory read.
///
/// This is significantly faster than calling `read_edge()` in a loop because it issues one
/// `mem.read()` instead of 4×count individual reads.
pub fn read_edges_bulk<M: Memory>(
    mem: &M,
    base: u64,
    start_slot: u64,
    count: u64,
) -> Vec<EdgeEntry> {
    if count == 0 {
        return Vec::new();
    }
    let byte_count = count as usize * EDGE_ENTRY_SIZE as usize;
    let mut buf = vec![0u8; byte_count];
    mem.read(base + start_slot * EDGE_ENTRY_SIZE, &mut buf);
    buf.chunks_exact(EDGE_ENTRY_SIZE as usize)
        .map(|chunk| EdgeEntry {
            target: u32::from_le_bytes(chunk[0..4].try_into().unwrap()),
            weight: f32::from_le_bytes(chunk[4..8].try_into().unwrap()),
            timestamp: u64::from_le_bytes(chunk[8..16].try_into().unwrap()),
            label_and_flags: u32::from_le_bytes(chunk[16..20].try_into().unwrap()),
            edge_id: u32::from_le_bytes(chunk[20..24].try_into().unwrap()),
        })
        .collect()
}

/// Reads an edge entry from the PMA edge region.
pub fn read_edge<M: Memory>(mem: &M, base: u64, slot: u64) -> EdgeEntry {
    let off = base + slot * EDGE_ENTRY_SIZE;
    let target = read_u32(mem, off);
    let mut w = [0u8; 4];
    mem.read(off + 4, &mut w);
    let weight = f32::from_le_bytes(w);
    let timestamp = read_u64(mem, off + 8);
    let label_and_flags = read_u32(mem, off + 16);
    let edge_id = read_u32(mem, off + 20);
    EdgeEntry {
        target,
        weight,
        timestamp,
        label_and_flags,
        edge_id,
    }
}

/// Writes an edge entry into the PMA edge region.
pub fn write_edge<M: Memory>(mem: &mut M, base: u64, slot: u64, e: &EdgeEntry) {
    let off = base + slot * EDGE_ENTRY_SIZE;
    write_u32(mem, off, e.target);
    mem.write(off + 4, &e.weight.to_le_bytes());
    write_u64(mem, off + 8, e.timestamp);
    write_u32(mem, off + 16, e.label_and_flags);
    write_u32(mem, off + 20, e.edge_id);
}

/// Reads an overflow log entry for a segment and slot.
pub fn read_log_entry<M: Memory>(mem: &M, base: u64, segment_id: u32, slot: u32) -> LogEntry {
    let per_seg = MAX_LOG_ENTRIES_PER_SEGMENT * LOG_ENTRY_SIZE;
    let off = base + segment_id as u64 * per_seg + slot as u64 * LOG_ENTRY_SIZE;
    let src = read_u32(mem, off);
    let dst = read_u32(mem, off + 4);
    let prev_offset = read_u32(mem, off + 8) as i32;
    let mut w = [0u8; 4];
    mem.read(off + 12, &mut w);
    let weight = f32::from_le_bytes(w);
    let timestamp = read_u64(mem, off + 16);
    let label_and_flags = read_u32(mem, off + 24);
    let edge_id = read_u32(mem, off + 28);
    LogEntry {
        src,
        dst,
        prev_offset,
        weight,
        timestamp,
        label_and_flags,
        edge_id,
    }
}

/// Writes an overflow log entry for a segment and slot.
pub fn write_log_entry<M: Memory>(
    mem: &mut M,
    base: u64,
    segment_id: u32,
    slot: u32,
    e: &LogEntry,
) {
    let per_seg = MAX_LOG_ENTRIES_PER_SEGMENT * LOG_ENTRY_SIZE;
    let off = base + segment_id as u64 * per_seg + slot as u64 * LOG_ENTRY_SIZE;
    write_u32(mem, off, e.src);
    write_u32(mem, off + 4, e.dst);
    write_u32(mem, off + 8, e.prev_offset as u32);
    mem.write(off + 12, &e.weight.to_le_bytes());
    write_u64(mem, off + 16, e.timestamp);
    write_u32(mem, off + 24, e.label_and_flags);
    write_u32(mem, off + 28, e.edge_id);
}

/// Reads the per-segment actual edge count.
pub fn read_seg_actual<M: Memory>(mem: &M, seg_tree_base: u64, seg_id: u32) -> u64 {
    read_u64(mem, seg_tree_base + seg_id as u64 * SEG_TREE_ENTRY_SIZE)
}

/// Writes the per-segment actual edge count.
pub fn write_seg_actual<M: Memory>(mem: &mut M, seg_tree_base: u64, seg_id: u32, value: u64) {
    write_u64(
        mem,
        seg_tree_base + seg_id as u64 * SEG_TREE_ENTRY_SIZE,
        value,
    )
}

/// Reads the per-segment allocated total slots for density accounting.
pub fn read_seg_total<M: Memory>(
    mem: &M,
    seg_tree_base: u64,
    segment_count: u32,
    seg_id: u32,
) -> u64 {
    let off = seg_tree_base
        + seg_tree_actual_region_size(segment_count as u64)
        + seg_id as u64 * SEG_TREE_ENTRY_SIZE;
    read_u64(mem, off)
}

/// Writes the per-segment allocated total slots for density accounting.
pub fn write_seg_total<M: Memory>(
    mem: &mut M,
    seg_tree_base: u64,
    segment_count: u32,
    seg_id: u32,
    value: u64,
) {
    let off = seg_tree_base
        + seg_tree_actual_region_size(segment_count as u64)
        + seg_id as u64 * SEG_TREE_ENTRY_SIZE;
    write_u64(mem, off, value)
}

/// Reads the fill count for a segment overflow log.
pub fn read_seg_log_fill<M: Memory>(mem: &M, seg_log_idx_base: u64, seg_id: u32) -> u32 {
    read_u32(mem, seg_log_idx_base + seg_id as u64 * SEG_LOG_INDEX_SIZE)
}

/// Writes the fill count for a segment overflow log.
pub fn write_seg_log_fill<M: Memory>(mem: &mut M, seg_log_idx_base: u64, seg_id: u32, value: u32) {
    write_u32(
        mem,
        seg_log_idx_base + seg_id as u64 * SEG_LOG_INDEX_SIZE,
        value,
    )
}

/// Reads and decodes the fixed-size stable header.
pub fn read_header<M: Memory>(mem: &M) -> StableHeader {
    let mut buf = [0u8; 4096];
    mem.read(HEADER_BASE, &mut buf);
    let mut reserved = [0u8; 4008];
    reserved.copy_from_slice(&buf[88..4096]);
    // Manual decode for stability and to avoid unsafe transmutes.
    StableHeader {
        magic: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        version: u16::from_le_bytes(buf[4..6].try_into().unwrap()),
        _pad: u16::from_le_bytes(buf[6..8].try_into().unwrap()),
        num_vertices: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        num_edges: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        elem_capacity: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
        segment_size: u32::from_le_bytes(buf[32..36].try_into().unwrap()),
        segment_count: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
        tree_height: u32::from_le_bytes(buf[40..44].try_into().unwrap()),
        next_edge_id: u32::from_le_bytes(buf[44..48].try_into().unwrap()),
        vertex_array_base: u64::from_le_bytes(buf[48..56].try_into().unwrap()),
        edge_array_base: u64::from_le_bytes(buf[56..64].try_into().unwrap()),
        seg_tree_base: u64::from_le_bytes(buf[64..72].try_into().unwrap()),
        seg_log_base: u64::from_le_bytes(buf[72..80].try_into().unwrap()),
        seg_log_idx_base: u64::from_le_bytes(buf[80..88].try_into().unwrap()),
        _reserved: reserved,
    }
}

/// Encodes and writes the fixed-size stable header.
pub fn write_header<M: Memory>(mem: &mut M, h: &StableHeader) {
    let mut buf = [0u8; 4096];
    buf[0..4].copy_from_slice(&h.magic.to_le_bytes());
    buf[4..6].copy_from_slice(&h.version.to_le_bytes());
    buf[6..8].copy_from_slice(&h._pad.to_le_bytes());
    buf[8..16].copy_from_slice(&h.num_vertices.to_le_bytes());
    buf[16..24].copy_from_slice(&h.num_edges.to_le_bytes());
    buf[24..32].copy_from_slice(&h.elem_capacity.to_le_bytes());
    buf[32..36].copy_from_slice(&h.segment_size.to_le_bytes());
    buf[36..40].copy_from_slice(&h.segment_count.to_le_bytes());
    buf[40..44].copy_from_slice(&h.tree_height.to_le_bytes());
    buf[44..48].copy_from_slice(&h.next_edge_id.to_le_bytes());
    buf[48..56].copy_from_slice(&h.vertex_array_base.to_le_bytes());
    buf[56..64].copy_from_slice(&h.edge_array_base.to_le_bytes());
    buf[64..72].copy_from_slice(&h.seg_tree_base.to_le_bytes());
    buf[72..80].copy_from_slice(&h.seg_log_base.to_le_bytes());
    buf[80..88].copy_from_slice(&h.seg_log_idx_base.to_le_bytes());
    buf[88..4096].copy_from_slice(&h._reserved);
    mem.write(HEADER_BASE, &buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::VecMemory;
    use gleaph_types::{STABLE_MAGIC, STABLE_VERSION};

    #[test]
    fn region_offsets_do_not_overlap() {
        let v = 1024;
        let cap = 8192;
        let s = 64;
        let edge = edge_array_base(v);
        let tree = seg_tree_base(v, cap);
        let log = seg_log_base(v, cap, s);
        let idx = seg_log_idx_base(v, cap, s);
        let end = total_memory_needed(v, cap, s);
        assert!(edge >= VERTEX_ARRAY_BASE + v * VERTEX_ENTRY_SIZE);
        assert!(tree >= edge + cap * EDGE_ENTRY_SIZE);
        assert!(log >= tree + seg_tree_actual_region_size(s) + seg_tree_total_region_size(s));
        assert!(idx >= log + s * MAX_LOG_ENTRIES_PER_SEGMENT * LOG_ENTRY_SIZE);
        assert!(end >= idx + s * SEG_LOG_INDEX_SIZE);
    }

    #[test]
    fn rw_round_trip() {
        let mut mem = VecMemory::with_size(total_memory_needed(8, 32, 4) as usize);
        let h = StableHeader {
            magic: STABLE_MAGIC,
            version: STABLE_VERSION,
            num_vertices: 8,
            num_edges: 3,
            elem_capacity: 32,
            segment_size: 2,
            segment_count: 4,
            tree_height: 2,
            vertex_array_base: VERTEX_ARRAY_BASE,
            edge_array_base: edge_array_base(8),
            seg_tree_base: seg_tree_base(8, 32),
            seg_log_base: seg_log_base(8, 32, 4),
            seg_log_idx_base: seg_log_idx_base(8, 32, 4),
            ..StableHeader::default()
        };
        write_header(&mut mem, &h);
        let h2 = read_header(&mem);
        assert_eq!(h.magic, h2.magic);
        assert_eq!(h.num_vertices, h2.num_vertices);

        let v = VertexEntry {
            edge_index: 5,
            degree: 2,
            log_offset: -1,
        };
        write_vertex(&mut mem, 3, &v);
        assert_eq!(read_vertex(&mem, 3), v);

        let e = EdgeEntry {
            target: 9,
            weight: 1.5,
            timestamp: 42,
            label_and_flags: gleaph_types::pack_label_and_flags(3, 0),
            edge_id: 7,
        };
        write_edge(&mut mem, h.edge_array_base, 7, &e);
        assert_eq!(read_edge(&mem, h.edge_array_base, 7), e);

        write_seg_actual(&mut mem, h.seg_tree_base, 1, 11);
        write_seg_total(&mut mem, h.seg_tree_base, h.segment_count, 1, 17);
        assert_eq!(read_seg_actual(&mem, h.seg_tree_base, 1), 11);
        assert_eq!(
            read_seg_total(&mem, h.seg_tree_base, h.segment_count, 1),
            17
        );
    }
}
