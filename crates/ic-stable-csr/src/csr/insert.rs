//! CSR slab insert (gap fill + right-slide) for a **single flat segment** (`segment_count == 1`).
//!
//! Higher-level orchestration (PMA sync, [`rebalance_decision`](crate::dgap::rebalance_decision),
//! [`DgapEdgeStore`](crate::dgap::DgapEdgeStore), resize) lives on [`DgapEdgeStore`](crate::dgap::DgapEdgeStore).

use ic_stable_structures::Memory;

use crate::traits::{CsrEdge, CsrVertex};
use crate::StableVec;

/// Local failure modes for one insert attempt before resize.
#[derive(Debug, PartialEq, Eq)]
pub enum CsrInsertError {
    VertexOutOfRange,
    SlabTooShort {
        have: usize,
        need: u64,
    },
    /// No free slot at the end of the slab to slide into; caller should double capacity.
    OutOfSlabSlots,
}

/// Local failure modes for removing one edge from the CSR slab.
#[derive(Debug, PartialEq, Eq)]
pub enum CsrRemoveError {
    VertexOutOfRange,
    EdgeIndexOutOfRange,
    SlabTooShort {
        have: usize,
        need: u64,
    },
    /// Vertex metadata and occupied tail disagree (e.g. `degree` past slab tail).
    RemovePositionOutOfRange,
}

#[inline]
fn next_vertex_base_inner<V: CsrVertex, G: FnMut(usize) -> V>(
    n: usize,
    v: usize,
    elem_capacity: u64,
    get: &mut G,
) -> u64 {
    if v + 1 < n {
        get(v + 1).base_slot_start()
    } else {
        elem_capacity
    }
}

#[inline]
fn rightmost_occupied_end_inner<V: CsrVertex, G: FnMut(usize) -> V>(n: usize, get: &mut G) -> u64 {
    let mut end = 0u64;
    for j in 0..n {
        let v = get(j);
        let b = v.base_slot_start();
        let d = v.degree() as u64;
        end = end.max(b.saturating_add(d));
    }
    end
}

/// Insert using random-access getters/setters (no full `Vec<V>` snapshot).
pub fn insert_edge_into_slab_inner<V: CsrVertex, E: CsrEdge, G, S>(
    n: usize,
    v: usize,
    mut get: G,
    mut set: S,
    edges: &mut [E],
    edge: E,
    elem_capacity: u64,
) -> Result<(), CsrInsertError>
where
    G: FnMut(usize) -> V,
    S: FnMut(usize, V),
{
    if v >= n {
        return Err(CsrInsertError::VertexOutOfRange);
    }
    if edges.len() < elem_capacity as usize {
        return Err(CsrInsertError::SlabTooShort {
            have: edges.len(),
            need: elem_capacity,
        });
    }

    let base = get(v).base_slot_start();
    let deg = get(v).degree();
    let insert_pos = base.saturating_add(deg as u64);
    let next_b = next_vertex_base_inner(n, v, elem_capacity, &mut get);

    if insert_pos >= elem_capacity {
        return Err(CsrInsertError::OutOfSlabSlots);
    }

    if insert_pos < next_b {
        edges[insert_pos as usize] = edge;
        set(v, get(v).with_degree(deg + 1));
        return Ok(());
    }

    let occ_end = rightmost_occupied_end_inner(n, &mut get);
    let rmax = occ_end.saturating_sub(1);
    if rmax.saturating_add(1) >= elem_capacity {
        return Err(CsrInsertError::OutOfSlabSlots);
    }

    let ip = insert_pos as usize;
    let rm = rmax as usize;
    for s in (ip..=rm).rev() {
        edges[s + 1] = edges[s];
    }
    edges[ip] = edge;

    for j in 0..n {
        let b = get(j).base_slot_start();
        if b >= insert_pos {
            set(j, get(j).with_base_slot_start(b.saturating_add(1)));
        }
    }
    set(v, get(v).with_degree(deg + 1));
    Ok(())
}

/// Remove one edge at `local_index` within `v`'s adjacency list (inverse of [`insert_edge_into_slab_inner`] slide rules).
pub fn remove_edge_from_slab_inner<V: CsrVertex, E: CsrEdge, G, S>(
    n: usize,
    v: usize,
    local_index: usize,
    mut get: G,
    mut set: S,
    edges: &mut [E],
    elem_capacity: u64,
) -> Result<(), CsrRemoveError>
where
    G: FnMut(usize) -> V,
    S: FnMut(usize, V),
{
    if v >= n {
        return Err(CsrRemoveError::VertexOutOfRange);
    }
    if edges.len() < elem_capacity as usize {
        return Err(CsrRemoveError::SlabTooShort {
            have: edges.len(),
            need: elem_capacity,
        });
    }

    let row = get(v);
    let deg = row.degree() as usize;
    if local_index >= deg {
        return Err(CsrRemoveError::EdgeIndexOutOfRange);
    }

    let base = row.base_slot_start();
    let remove_pos = base.saturating_add(local_index as u64);
    let occ_end = rightmost_occupied_end_inner(n, &mut get);
    if remove_pos >= occ_end {
        return Err(CsrRemoveError::RemovePositionOutOfRange);
    }

    let end = occ_end as usize;
    let rp = remove_pos as usize;
    for s in rp..end.saturating_sub(1) {
        edges[s] = edges[s + 1];
    }

    for j in 0..n {
        let b = get(j).base_slot_start();
        if b > remove_pos {
            set(j, get(j).with_base_slot_start(b.saturating_sub(1)));
        }
    }
    set(v, get(v).with_degree((deg - 1) as u32));
    Ok(())
}

/// Remove using a dense [`StableVec`] vertex table (same append-only convention as CSR `M_v`).
pub fn remove_edge_from_slab_column<V, M, E>(
    col: &StableVec<V, M>,
    edges: &mut [E],
    v: usize,
    local_index: usize,
    elem_capacity: u64,
) -> Result<(), CsrRemoveError>
where
    V: CsrVertex,
    M: Memory,
    E: CsrEdge,
{
    let n = col.len() as usize;
    remove_edge_from_slab_inner(
        n,
        v,
        local_index,
        |i| {
            col.get(i as u64)
                .unwrap_or_else(|| panic!("vertex index {i} out of range for len {n}"))
        },
        |i, x| {
            if col.get(i as u64).is_none() {
                panic!("vertex set failed: index {i} out of range for len {n}");
            }
            col.set(i as u64, &x)
        },
        edges,
        elem_capacity,
    )
}

/// Test / bench helper: contiguous `&mut [V]` without a column type.
pub fn remove_edge_from_slab<V: CsrVertex, E: CsrEdge>(
    vertices: &mut [V],
    edges: &mut [E],
    v: usize,
    local_index: usize,
    elem_capacity: u64,
) -> Result<(), CsrRemoveError> {
    let n = vertices.len();
    if v >= n {
        return Err(CsrRemoveError::VertexOutOfRange);
    }
    if edges.len() < elem_capacity as usize {
        return Err(CsrRemoveError::SlabTooShort {
            have: edges.len(),
            need: elem_capacity,
        });
    }

    let row = vertices[v];
    let deg = row.degree() as usize;
    if local_index >= deg {
        return Err(CsrRemoveError::EdgeIndexOutOfRange);
    }

    let base = row.base_slot_start();
    let remove_pos = base.saturating_add(local_index as u64);
    let mut occ_end = 0u64;
    for j in 0..n {
        let vr = vertices[j];
        let b = vr.base_slot_start();
        let d = vr.degree() as u64;
        occ_end = occ_end.max(b.saturating_add(d));
    }
    if remove_pos >= occ_end {
        return Err(CsrRemoveError::RemovePositionOutOfRange);
    }

    let end = occ_end as usize;
    let rp = remove_pos as usize;
    for s in rp..end.saturating_sub(1) {
        edges[s] = edges[s + 1];
    }

    for j in 0..n {
        let b = vertices[j].base_slot_start();
        if b > remove_pos {
            vertices[j] = vertices[j].with_base_slot_start(b.saturating_sub(1));
        }
    }
    vertices[v] = vertices[v].with_degree((deg - 1) as u32);
    Ok(())
}

/// Insert using a dense [`StableVec`] vertex table (same append-only convention as CSR `M_v`).
pub fn insert_edge_into_slab_column<V, M, E>(
    col: &StableVec<V, M>,
    edges: &mut [E],
    v: usize,
    edge: E,
    elem_capacity: u64,
) -> Result<(), CsrInsertError>
where
    V: CsrVertex,
    M: Memory,
    E: CsrEdge,
{
    let n = col.len() as usize;
    insert_edge_into_slab_inner(
        n,
        v,
        |i| {
            col.get(i as u64)
                .unwrap_or_else(|| panic!("vertex index {i} out of range for len {n}"))
        },
        |i, x| {
            if col.get(i as u64).is_none() {
                panic!("vertex set failed: index {i} out of range for len {n}");
            }
            col.set(i as u64, &x)
        },
        edges,
        edge,
        elem_capacity,
    )
}

/// Test / bench helper: contiguous `&mut [V]` without a column type.
pub fn insert_edge_into_slab<V: CsrVertex, E: CsrEdge>(
    vertices: &mut [V],
    edges: &mut [E],
    v: usize,
    edge: E,
    elem_capacity: u64,
) -> Result<(), CsrInsertError> {
    let n = vertices.len();
    if v >= n {
        return Err(CsrInsertError::VertexOutOfRange);
    }
    if edges.len() < elem_capacity as usize {
        return Err(CsrInsertError::SlabTooShort {
            have: edges.len(),
            need: elem_capacity,
        });
    }

    let base = vertices[v].base_slot_start();
    let deg = vertices[v].degree();
    let insert_pos = base.saturating_add(deg as u64);
    let next_b = if v + 1 < n {
        vertices[v + 1].base_slot_start()
    } else {
        elem_capacity
    };

    if insert_pos >= elem_capacity {
        return Err(CsrInsertError::OutOfSlabSlots);
    }

    if insert_pos < next_b {
        edges[insert_pos as usize] = edge;
        vertices[v] = vertices[v].with_degree(deg + 1);
        return Ok(());
    }

    let mut occ_end = 0u64;
    for row in vertices.iter() {
        let b = row.base_slot_start();
        let d = row.degree() as u64;
        occ_end = occ_end.max(b.saturating_add(d));
    }
    let rmax = occ_end.saturating_sub(1);
    if rmax.saturating_add(1) >= elem_capacity {
        return Err(CsrInsertError::OutOfSlabSlots);
    }

    let ip = insert_pos as usize;
    let rm = rmax as usize;
    for s in (ip..=rm).rev() {
        edges[s + 1] = edges[s];
    }
    edges[ip] = edge;

    for j in 0..n {
        let b = vertices[j].base_slot_start();
        if b >= insert_pos {
            vertices[j] = vertices[j].with_base_slot_start(b.saturating_add(1));
        }
    }
    vertices[v] = vertices[v].with_degree(deg + 1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{CsrEdge, CsrVertex};
    use crate::VertexId;
    use std::borrow::Cow;

    use crate::{Bound, Storable};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TV {
        slot_base: u64,
        deg: u32,
        log_head: i32,
    }

    impl Storable for TV {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&self.slot_base.to_le_bytes());
            b[8..12].copy_from_slice(&self.deg.to_le_bytes());
            b[12..16].copy_from_slice(&self.log_head.to_le_bytes());
            Cow::Owned(b.to_vec())
        }
        fn into_bytes(self) -> Vec<u8> {
            self.to_bytes().into_owned()
        }
        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            let s = bytes.as_ref();
            Self {
                slot_base: u64::from_le_bytes(s[0..8].try_into().unwrap()),
                deg: u32::from_le_bytes(s[8..12].try_into().unwrap()),
                log_head: i32::from_le_bytes(s[12..16].try_into().unwrap()),
            }
        }
        const BOUND: Bound = Bound::Bounded {
            max_size: 16,
            is_fixed_size: true,
        };
    }

    impl CsrVertex for TV {
        fn base_slot_start(&self) -> u64 {
            self.slot_base
        }
        fn degree(&self) -> u32 {
            self.deg
        }
        fn with_base_slot_start(self, start: u64) -> Self {
            Self {
                slot_base: start,
                ..self
            }
        }
        fn with_degree(self, degree: u32) -> Self {
            Self {
                deg: degree,
                ..self
            }
        }
        fn log_head(self) -> i32 {
            self.log_head
        }
        fn with_log_head(self, idx: i32) -> Self {
            Self {
                log_head: idx,
                ..self
            }
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct TE(u8);

    impl CsrEdge for TE {
        const EDGE_BYTES: usize = 1;

        fn read_from(bytes: &[u8]) -> Self {
            Self(bytes[0])
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0] = self.0;
        }

        fn neighbor_vid(&self) -> VertexId {
            self.0 as VertexId
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self(vid as u8)
        }
    }

    #[test]
    fn insert_uses_gap_before_next_vertex() {
        let mut v = [
            TV {
                slot_base: 0,
                deg: 1,
                log_head: -1,
            },
            TV {
                slot_base: 4,
                deg: 0,
                log_head: -1,
            },
        ];
        let mut e = vec![TE(1), TE(0), TE(0), TE(0), TE(0)];
        insert_edge_into_slab(&mut v, &mut e, 0, TE(2), 5).unwrap();
        assert_eq!(v[0].deg, 2);
        assert_eq!(e[1].0, 2);
    }

    #[test]
    fn insert_packs_then_slides_and_bumps_bases() {
        let mut v = [
            TV {
                slot_base: 0,
                deg: 2,
                log_head: -1,
            },
            TV {
                slot_base: 2,
                deg: 1,
                log_head: -1,
            },
        ];
        let mut e = vec![TE(1), TE(2), TE(3), TE(0), TE(0), TE(0), TE(0), TE(0)];
        insert_edge_into_slab(&mut v, &mut e, 0, TE(9), 8).unwrap();
        assert_eq!(v[0].deg, 3);
        assert_eq!(v[0].slot_base, 0);
        assert_eq!(v[1].slot_base, 3);
        assert_eq!(e[0].0, 1);
        assert_eq!(e[1].0, 2);
        assert_eq!(e[2].0, 9);
        assert_eq!(e[3].0, 3);
    }

    #[test]
    fn remove_inverse_of_slide_and_bumps_bases() {
        let mut v = [
            TV {
                slot_base: 0,
                deg: 3,
                log_head: -1,
            },
            TV {
                slot_base: 3,
                deg: 1,
                log_head: -1,
            },
        ];
        let mut e = vec![TE(1), TE(2), TE(9), TE(3), TE(0), TE(0), TE(0), TE(0)];
        remove_edge_from_slab(&mut v, &mut e, 0, 1, 8).unwrap();
        assert_eq!(v[0].deg, 2);
        assert_eq!(v[0].slot_base, 0);
        assert_eq!(v[1].slot_base, 2);
        assert_eq!(e[0].0, 1);
        assert_eq!(e[1].0, 9);
        assert_eq!(e[2].0, 3);
    }
}
