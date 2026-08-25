//! Durable rebuild-pool region (`VECTOR_REBUILD_POOL`, MemoryId 18) — the single owner of the
//! frozen Sampling/Training candidate pool and the Training centroid work area (ADR 0033
//! implementation).
//!
//! The region is **raw** (no stable collection) with a minimal fixed-size header followed by two
//! fixed-width arrays plus, for a two-level (`levels = 2`) rebuild, a per-row coarse-id array:
//!
//! ```text
//! [header 96 B][candidate slots: pool_capacity × (pad_stride + 8)]
//! [centroid area: work_centroids × centroid_stride]
//! [coarse ids (two-level only): pool_capacity × 4]
//! ```
//!
//! The header carries magic + format version + the binding `(index_id, pad_stride,
//! work_centroids, centroid_stride, two_level)` + the lengths (`pool_len`, `pool_capacity`,
//! `centroid_count`, `assigned_len`). Every phase that touches the pool re-validates the whole
//! header fail-closed against the definition-derived geometry and the durable lifecycle scalars,
//! so an interrupted or upgraded rebuild either resumes from an exactly consistent pool or returns
//! [`VectorCanisterError::RebuildPoolInvalid`] before any mutation.
//!
//! **Coarse ids (Slice 5).** A two-level rebuild persists each pool row's nearest coarse subtree
//! id once, after coarse training converges and before any fine subtree job runs. The assignment
//! pass is a deterministic single scan (nearest coarse centroid, lowest-id tie-break), so fine
//! jobs resume from exactly the same membership without rescoring the pool. A flat rebuild
//! reserves no coarse-id area at all, so its region capacity numbers are byte-identical to the
//! pre-Slice-5 layout.
//!
//! The region is **single-tenant**: it holds at most one in-flight rebuild's pool. A second index
//! cannot start a rebuild while another index binds the region (admission serializes cross-index
//! rebuilds; per-index lifecycle semantics are unchanged). Rows are written once by `Sampling`
//! appends, read by `Sampling` dedup (streamed, never materialized as a whole) and by `Training`
//! k-means iterations, and the whole binding is released (header zeroed) when the lifecycle leaves
//! `Sampling`/`Training` for good: abort entry, teardown completion, `Failed`, and the coordinated
//! definition-domain reset.
//!
//! Layout cutover note: this is a fresh pre-production layout (format version 2, breaking over
//! version 1's no-coarse-id layout). There is no migration and no compatibility reader; reinstall
//! is required after any layout change.

use super::memory::{Memory, rebuild_pool_memory};
use crate::records::RebuildCandidate;
use gleaph_graph_kernel::vector_index::VectorCanisterError;
use ic_stable_structures::Memory as _;

/// Candidate aux width (the page-store row-meta aux carried beside the stored bytes).
const AUX_LEN: usize = 8;

/// Magic bytes of a bound pool header (`V`ector `R`ebuild `P`ool).
const MAGIC: [u8; 3] = *b"VRP";

/// Format version of the pool region layout. Version 2 (Slice 5) added the optional per-row
/// coarse-id area and the `assigned_len` header field; version 3 (Slice 6) added the `code_tier`
/// flag byte so the shadow generation's row geometry survives into `Building` without widening
/// the lifecycle records. Earlier versions are rejected.
const VERSION: u8 = 3;

/// Fixed header size. Offsets:
/// `0..3` magic, `3` version, `4..8` index_id, `8..12` pad_stride, `12..20` pool_len,
/// `20..28` pool_capacity, `28..32` work_centroids, `32..36` centroid_stride, `36..40`
/// centroid_count, `40..44` assigned_len, `44` two_level flag, `45` code_tier flag (Slice 6:
/// carried from rebuild start to the `Building` transition — training phases never consult it),
/// `46..96` reserved zero.
pub(crate) const POOL_HEADER_SIZE: u64 = 96;

/// Per-row width of the coarse-id array (`u32` LE) reserved for a two-level rebuild. A flat
/// rebuild reserves none, keeping its region capacity identical to the pre-Slice-5 layout.
pub(crate) const COARSE_ID_WIDTH: u64 = 4;

/// Total byte budget of the pool region (header + slots + centroid area). Mirrors the scale of
/// the retired combined-state envelope so admission behavior stays comparable; unlike that
/// envelope it bounds a physical region, not a Candid encoding.
pub(crate) const REGION_BYTES: u64 = 8 * 1024 * 1024;

/// Stable-memory page size (mirrors the slab page store's growth granularity).
const WASM_PAGE_SIZE: u64 = 65_536;

/// Streaming read granularity for whole-pool passes (dedup hashing): several rows per stable read,
/// never retaining more than one chunk of row bytes on the heap.
const STREAM_CHUNK_BYTES: usize = 128 * 1024;

/// Fail-closed resume-validation failure modes. Callers map every variant to
/// [`VectorCanisterError::RebuildPoolInvalid`]; the distinction exists so tests can pin the exact
/// violated invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PoolValidationError {
    /// No header: fresh region or a released binding.
    Absent,
    /// Magic or format version mismatch.
    CorruptHeader,
    /// The region is bound to a different index than the caller's.
    ForeignIndex { bound: u32 },
    /// Header geometry disagrees with the definition-derived expectation, or the recorded
    /// capacities are inconsistent with the region budget.
    GeometryMismatch,
    /// A recorded length exceeds its array capacity.
    LengthMismatch,
}

impl From<PoolValidationError> for VectorCanisterError {
    fn from(_: PoolValidationError) -> Self {
        VectorCanisterError::RebuildPoolInvalid
    }
}

/// Validated snapshot of the pool header taken by [`open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenPool {
    /// Candidates accumulated so far (`<= pool_capacity`).
    pub pool_len: u32,
    /// Pool rows whose coarse-id entry is persisted (`<= pool_len`). `0` until the two-level
    /// coarse-assignment pass runs; a flat rebuild keeps it at `0` forever.
    pub assigned_len: u32,
    /// Code tier of the shadow generation (Slice 6): persisted at `begin` and consumed by the
    /// transition into `Building` so the shadow pages are reserved with the right geometry.
    pub code_tier: bool,
}

struct PoolGeometry {
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    /// Whether the coarse-id array is reserved (two-level rebuilds only).
    two_level: bool,
    pool_capacity: u64,
}

/// Full durable bytes one candidate slot occupies: its row width plus, for a two-level rebuild,
/// the coarse-id cell ([`COARSE_ID_WIDTH`]).
fn per_row_bytes(pad_stride: u32, two_level: bool) -> u64 {
    u64::from(pad_stride) + AUX_LEN as u64 + if two_level { COARSE_ID_WIDTH } else { 0 }
}

impl PoolGeometry {
    fn row_width(&self) -> u64 {
        u64::from(self.pad_stride) + AUX_LEN as u64
    }

    fn slots_range_end(&self) -> u64 {
        POOL_HEADER_SIZE + self.pool_capacity * self.row_width()
    }

    fn centroid_area_bytes(&self) -> u64 {
        u64::from(self.work_centroids) * u64::from(self.centroid_stride)
    }

    fn coarse_ids_range_start(&self) -> u64 {
        self.slots_range_end() + self.centroid_area_bytes()
    }
}

/// Slot capacity for the given geometry under [`REGION_BYTES`], or `None` when the geometry cannot
/// host at least `work_centroids` candidates (overflow or budget exhaustion) — the fail-closed
/// admission bound that replaced the Candid-envelope constraint. A two-level rebuild additionally
/// reserves [`COARSE_ID_WIDTH`] bytes per row, so its capacity is slightly smaller than a flat
/// rebuild's at the same strides.
pub(crate) fn pool_capacity_for(
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Option<u64> {
    let row_total = per_row_bytes(pad_stride, two_level);
    let centroid_bytes = u64::from(work_centroids).checked_mul(u64::from(centroid_stride))?;
    let budget = REGION_BYTES.checked_sub(POOL_HEADER_SIZE)?;
    if centroid_bytes > budget {
        return None;
    }
    let available = budget - centroid_bytes;
    let capacity = available / row_total.max(1);
    if capacity < u64::from(work_centroids) {
        return None;
    }
    Some(capacity)
}

fn pool_memory() -> Memory {
    rebuild_pool_memory()
}

/// Reads the header block. The bool is `true` when the region is unwritten or fully zeroed
/// (no binding).
fn read_header(mem: &Memory) -> ([u8; POOL_HEADER_SIZE as usize], bool) {
    let mut buf = [0u8; POOL_HEADER_SIZE as usize];
    if mem.size() > 0 {
        mem.read(0, &mut buf);
    }
    let absent = buf.iter().all(|b| *b == 0);
    (buf, absent)
}

fn decode_u32(buf: &[u8], range: std::ops::Range<usize>) -> u32 {
    u32::from_le_bytes(buf[range].try_into().expect("header u32 field"))
}

fn decode_u64(buf: &[u8], range: std::ops::Range<usize>) -> u64 {
    u64::from_le_bytes(buf[range].try_into().expect("header u64 field"))
}

/// Parses and fully validates the header against the expected geometry. Returns the derived
/// geometry plus the recorded lengths.
#[allow(clippy::too_many_arguments)]
fn validate_header(
    mem: &Memory,
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Result<(PoolGeometry, u64, u32, u32, bool), PoolValidationError> {
    let (buf, absent) = read_header(mem);
    if absent {
        return Err(PoolValidationError::Absent);
    }
    if buf[0..3] != MAGIC || buf[3] != VERSION {
        return Err(PoolValidationError::CorruptHeader);
    }
    let bound = decode_u32(&buf, 4..8);
    if bound != index_id {
        return Err(PoolValidationError::ForeignIndex { bound });
    }
    if decode_u32(&buf, 8..12) != pad_stride
        || decode_u32(&buf, 28..32) != work_centroids
        || decode_u32(&buf, 32..36) != centroid_stride
        || (buf[44] != 0) != two_level
        || buf[45] > 1
    {
        return Err(PoolValidationError::GeometryMismatch);
    }
    let Some(capacity) = pool_capacity_for(pad_stride, work_centroids, centroid_stride, two_level)
    else {
        // The recorded geometry itself cannot fit the region budget: tampered or stale header.
        return Err(PoolValidationError::GeometryMismatch);
    };
    if decode_u64(&buf, 20..28) != capacity {
        return Err(PoolValidationError::GeometryMismatch);
    }
    let geometry = PoolGeometry {
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
        pool_capacity: capacity,
    };
    let pool_len = decode_u64(&buf, 12..20);
    if pool_len > capacity {
        return Err(PoolValidationError::LengthMismatch);
    }
    let centroid_count = decode_u32(&buf, 36..40);
    if u64::from(centroid_count) > u64::from(work_centroids) {
        return Err(PoolValidationError::LengthMismatch);
    }
    let assigned_len = decode_u32(&buf, 40..44);
    if !two_level && assigned_len != 0 {
        return Err(PoolValidationError::LengthMismatch);
    }
    if u64::from(assigned_len) > pool_len {
        return Err(PoolValidationError::LengthMismatch);
    }
    let code_tier = buf[45] == 1;
    Ok((geometry, pool_len, centroid_count, assigned_len, code_tier))
}

/// The index the region is currently bound to, or `None` when absent/corrupt. Used by rebuild
/// admission to serialize cross-index rebuilds; a corrupt header never blocks a fresh `begin`.
pub(crate) fn bound_index() -> Option<u32> {
    let mem = pool_memory();
    let (buf, absent) = read_header(&mem);
    if absent || buf[0..3] != MAGIC || buf[3] != VERSION {
        return None;
    }
    Some(decode_u32(&buf, 4..8))
}

/// Grows the region and writes a fresh, empty header bound to `(index_id, geometry)`. Overwrites
/// any previous binding; the caller must have established that no other index owns the region.
/// Fails closed before any write when the geometry exceeds the region budget. `code_tier` is the
/// shadow generation's Slice 6 flag, persisted here because `Sampling`/`Training` records stay
/// shape-minimal — only `Building` (via [`OpenPool::code_tier`]) consumes it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn begin(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
    code_tier: bool,
) -> Result<(), VectorCanisterError> {
    let Some(capacity) = pool_capacity_for(pad_stride, work_centroids, centroid_stride, two_level)
    else {
        return Err(VectorCanisterError::InvalidRebuildParams);
    };
    let geometry = PoolGeometry {
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
        pool_capacity: capacity,
    };
    let needed = if two_level {
        geometry.coarse_ids_range_start() + capacity * COARSE_ID_WIDTH
    } else {
        geometry.slots_range_end() + geometry.centroid_area_bytes()
    };
    let mem = pool_memory();
    let size_bytes = mem
        .size()
        .checked_mul(WASM_PAGE_SIZE)
        .expect("pool region address space overflow");
    if size_bytes < needed {
        let delta_pages = (needed - size_bytes).div_ceil(WASM_PAGE_SIZE);
        if mem.grow(delta_pages) == -1 {
            return Err(VectorCanisterError::StableGrowFailed);
        }
    }

    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    header[0..3].copy_from_slice(&MAGIC);
    header[3] = VERSION;
    header[4..8].copy_from_slice(&index_id.to_le_bytes());
    header[8..12].copy_from_slice(&pad_stride.to_le_bytes());
    header[12..20].copy_from_slice(&0u64.to_le_bytes());
    header[20..28].copy_from_slice(&capacity.to_le_bytes());
    header[28..32].copy_from_slice(&work_centroids.to_le_bytes());
    header[32..36].copy_from_slice(&centroid_stride.to_le_bytes());
    header[36..40].copy_from_slice(&0u32.to_le_bytes());
    header[40..44].copy_from_slice(&0u32.to_le_bytes());
    header[44] = u8::from(two_level);
    header[45] = u8::from(code_tier);
    mem.write(0, &header);
    Ok(())
}

/// Validates the header against the expected geometry (fail-closed resume check).
#[allow(clippy::too_many_arguments)]
pub(crate) fn open(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Result<OpenPool, PoolValidationError> {
    let mem = pool_memory();
    let (_, pool_len, _, assigned_len, code_tier) = validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    let pool_len = u32::try_from(pool_len).map_err(|_| PoolValidationError::LengthMismatch)?;
    Ok(OpenPool {
        pool_len,
        assigned_len,
        code_tier,
    })
}

/// Discards all accumulated candidate rows, keeping the binding and the centroid area. Used when
/// a subject-scan restart invalidates the accumulation tied to the old scan geometry. The coarse
/// assignment is reset with the rows it described.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reset_pool_rows(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    header[12..20].copy_from_slice(&0u64.to_le_bytes());
    header[40..44].copy_from_slice(&0u32.to_le_bytes());
    mem.write(0, &header);
    Ok(())
}

/// Appends `rows` to the candidate array and advances the header length. Every row must carry the
/// bound pad-stride width. The header advances strictly after the row bytes are persisted, and
/// both writes commit atomically with the caller's message. Appends are only legal before any
/// coarse assignment exists (`assigned_len == 0`); sampling never runs after it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn append_rows(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
    rows: &[RebuildCandidate],
) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    let (geo, pool_len, _, assigned_len, _) = validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    let pool_len = usize::try_from(pool_len).map_err(|_| PoolValidationError::LengthMismatch)?;
    if assigned_len != 0 {
        return Err(PoolValidationError::LengthMismatch);
    }
    if rows.is_empty() {
        return Ok(());
    }
    if pool_len + rows.len() > usize::try_from(geo.pool_capacity).unwrap_or(usize::MAX) {
        // Defense in depth: the sampling policy cap equals this capacity, so reaching here means
        // corruption or a policy bug — fail closed instead of overflowing the array.
        return Err(PoolValidationError::LengthMismatch);
    }
    let mut image = Vec::with_capacity(rows.len() * (pad_stride as usize + AUX_LEN));
    for row in rows {
        if row.stored.len() != pad_stride as usize {
            return Err(PoolValidationError::GeometryMismatch);
        }
        image.extend_from_slice(&row.stored);
        image.extend_from_slice(&row.aux);
    }
    let base = POOL_HEADER_SIZE + pool_len as u64 * geo.row_width();
    mem.write(base, &image);

    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    header[12..20].copy_from_slice(&((pool_len + rows.len()) as u64).to_le_bytes());
    mem.write(0, &header);
    Ok(())
}

/// Reads one candidate row (call only after [`open`] validated the binding).
pub(crate) fn read_row(index: u32, pad_stride: u32) -> RebuildCandidate {
    let mem = pool_memory();
    let row_width = u64::from(pad_stride) + AUX_LEN as u64;
    let mut raw = vec![0u8; pad_stride as usize + AUX_LEN];
    mem.read(POOL_HEADER_SIZE + u64::from(index) * row_width, &mut raw);
    let mut aux = [0u8; AUX_LEN];
    aux.copy_from_slice(&raw[pad_stride as usize..]);
    raw.truncate(pad_stride as usize);
    RebuildCandidate { stored: raw, aux }
}

/// Streams every candidate row through `f` in bounded chunks without retaining more than one
/// chunk of row bytes. Call only after [`open`] validated the binding.
pub(crate) fn for_each_row(
    pad_stride: u32,
    mut f: impl FnMut(u32, &[u8], [u8; AUX_LEN]),
) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    let pool_len = decode_u64(&header, 12..20);
    let row_width = u64::from(pad_stride) + AUX_LEN as u64;
    if row_width == 0 {
        return Err(PoolValidationError::GeometryMismatch);
    }
    let rows_per_chunk = (STREAM_CHUNK_BYTES as u64 / row_width).max(1);
    let mut chunk = vec![0u8; (rows_per_chunk * row_width) as usize];
    let mut next = 0u64;
    while next < pool_len {
        let count = rows_per_chunk.min(pool_len - next);
        let bytes = count as usize * row_width as usize;
        mem.read(POOL_HEADER_SIZE + next * row_width, &mut chunk[..bytes]);
        for i in 0..count as usize {
            let base = i * row_width as usize;
            let stored = &chunk[base..base + pad_stride as usize];
            let mut aux = [0u8; AUX_LEN];
            aux.copy_from_slice(&chunk[base + pad_stride as usize..base + row_width as usize]);
            f((next + i as u64) as u32, stored, aux);
        }
        next += count;
    }
    Ok(())
}

/// Loads every candidate row into memory (Training bulk input; one large stable read per call).
/// Call only after [`open`] validated the binding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn load_rows(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Result<Vec<RebuildCandidate>, PoolValidationError> {
    let mem = pool_memory();
    let (_, pool_len, _, _, _) = validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    let pool_len = usize::try_from(pool_len).map_err(|_| PoolValidationError::LengthMismatch)?;
    let row_width = u64::from(pad_stride) + AUX_LEN as u64;
    let mut raw = vec![0u8; pool_len * row_width as usize];
    mem.read(POOL_HEADER_SIZE, &mut raw);
    let mut rows = Vec::with_capacity(pool_len);
    for i in 0..pool_len {
        let base = i * row_width as usize;
        let mut aux = [0u8; AUX_LEN];
        aux.copy_from_slice(&raw[base + pad_stride as usize..base + row_width as usize]);
        rows.push(RebuildCandidate {
            stored: raw[base..base + pad_stride as usize].to_vec(),
            aux,
        });
    }
    Ok(rows)
}

/// Persists the per-row nearest-coarse assignment (`ids.len() == pool_len`, values `< nlist`) and
/// records `assigned_len`. Two-level rebuilds only; the write is idempotent only when overwriting
/// the exact same length (a resume of an interrupted pass rewrites all ids before any fine job
/// reads them). Call only after [`open`] validated the binding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn put_coarse_ids(
    index_id: u32,
    pad_stride: u32,
    nlist_coarse: u32,
    centroid_stride: u32,
    ids: &[u32],
) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    let (geo, pool_len, _, _, _) = validate_header(
        &mem,
        index_id,
        pad_stride,
        nlist_coarse,
        centroid_stride,
        true,
    )?;
    if !geo.two_level {
        return Err(PoolValidationError::GeometryMismatch);
    }
    if ids.len() != usize::try_from(pool_len).map_err(|_| PoolValidationError::LengthMismatch)? {
        return Err(PoolValidationError::LengthMismatch);
    }
    if ids.iter().any(|&id| id >= nlist_coarse) {
        return Err(PoolValidationError::GeometryMismatch);
    }
    let base = geo.coarse_ids_range_start();
    // Write in bounded chunks so a huge pool does not materialize one giant image.
    const CHUNK: usize = 16 * 1024;
    for (chunk_index, chunk) in ids.chunks(CHUNK).enumerate() {
        let mut image = Vec::with_capacity(chunk.len() * COARSE_ID_WIDTH as usize);
        for id in chunk {
            image.extend_from_slice(&id.to_le_bytes());
        }
        mem.write(
            base + (chunk_index * CHUNK) as u64 * COARSE_ID_WIDTH,
            &image,
        );
    }
    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    header[40..44].copy_from_slice(&(ids.len() as u32).to_le_bytes());
    mem.write(0, &header);
    Ok(())
}

/// Streams the durable per-row coarse ids through `f` in index order. Two-level rebuilds only;
/// callers must have observed [`OpenPool::assigned_len`] covering the whole pool first.
pub(crate) fn for_each_coarse_id(mut f: impl FnMut(u32, u32)) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    if buf_is_absent(&header) || header[0..3] != MAGIC || header[3] != VERSION || header[44] == 0 {
        return Err(PoolValidationError::GeometryMismatch);
    }
    let pool_len = decode_u64(&header, 12..20);
    let assigned_len = decode_u32(&header, 40..44);
    // Streaming requires a complete, non-empty assignment: `put_coarse_ids` always covers the
    // whole pool exactly, so a partial count or an emptied pool (its assignment was dropped by
    // `reset_pool_rows`) is a corrupt/stale state, not a valid empty stream.
    if pool_len == 0 || u64::from(assigned_len) != pool_len {
        return Err(PoolValidationError::GeometryMismatch);
    }
    let geometry = PoolGeometry {
        pad_stride: decode_u32(&header, 8..12),
        work_centroids: decode_u32(&header, 28..32),
        centroid_stride: decode_u32(&header, 32..36),
        two_level: true,
        pool_capacity: decode_u64(&header, 20..28),
    };
    let base = geometry.coarse_ids_range_start();
    const CHUNK: usize = 16 * 1024;
    let mut next = 0u64;
    while next < pool_len {
        let count = (CHUNK as u64).min(pool_len - next);
        let bytes = count as usize * COARSE_ID_WIDTH as usize;
        let mut raw = vec![0u8; bytes];
        mem.read(base + next * COARSE_ID_WIDTH, &mut raw);
        for (i, cell) in raw
            .as_chunks::<{ COARSE_ID_WIDTH as usize }>()
            .0
            .iter()
            .enumerate()
        {
            f((next + i as u64) as u32, decode_u32(cell, 0..4));
        }
        next += count;
    }
    Ok(())
}

fn buf_is_absent(buf: &[u8]) -> bool {
    buf.iter().all(|b| *b == 0)
}

/// Writes the Training centroid work area (up to `work_centroids` canonical-f32 centroids) and
/// records the count. Accepts a fresh area (`0`) or an overwrite of a complete set of the same
/// length; flat training and each two-level subtree job write fixed-length sets.
#[allow(clippy::too_many_arguments)]
pub(crate) fn put_centroids(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
    centroids: &[Vec<u8>],
) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    let (geo, _, centroid_count, _, _) = validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    if centroids.is_empty() || centroids.len() > work_centroids as usize {
        return Err(PoolValidationError::LengthMismatch);
    }
    if centroid_count != 0 && centroid_count != centroids.len() as u32 {
        return Err(PoolValidationError::LengthMismatch);
    }
    for centroid in centroids {
        if centroid.len() != centroid_stride as usize {
            return Err(PoolValidationError::GeometryMismatch);
        }
    }
    let base = geo.slots_range_end();
    for (i, centroid) in centroids.iter().enumerate() {
        mem.write(base + i as u64 * u64::from(centroid_stride), centroid);
    }
    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    header[36..40].copy_from_slice(&(centroids.len() as u32).to_le_bytes());
    mem.write(0, &header);
    Ok(())
}

/// Clears the recorded centroid count so the next phase can seed a set of a different length
/// (the two-level transition from the coarse set to per-subtree fine sets).
#[allow(clippy::too_many_arguments)]
pub(crate) fn reset_centroids(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Result<(), PoolValidationError> {
    let mem = pool_memory();
    validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    let mut header = [0u8; POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    header[36..40].copy_from_slice(&0u32.to_le_bytes());
    mem.write(0, &header);
    Ok(())
}

/// Reads the Training centroid work area: an empty `Vec` when seeding has not happened yet, or
/// the recorded set of decoded bytes otherwise. Call only after [`open`] validated the binding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn get_centroids(
    index_id: u32,
    pad_stride: u32,
    work_centroids: u32,
    centroid_stride: u32,
    two_level: bool,
) -> Result<Vec<Vec<u8>>, PoolValidationError> {
    let mem = pool_memory();
    let (geo, _, centroid_count, _, _) = validate_header(
        &mem,
        index_id,
        pad_stride,
        work_centroids,
        centroid_stride,
        two_level,
    )?;
    if centroid_count == 0 {
        return Ok(Vec::new());
    }
    if centroid_count > work_centroids {
        return Err(PoolValidationError::LengthMismatch);
    }
    let base = geo.slots_range_end();
    let mut out = Vec::with_capacity(centroid_count as usize);
    for p in 0..centroid_count {
        let mut buf = vec![0u8; centroid_stride as usize];
        mem.read(base + u64::from(p) * u64::from(centroid_stride), &mut buf);
        out.push(buf);
    }
    Ok(out)
}

/// Releases the binding by zeroing the header block. Idempotent; the region keeps its grown size
/// (stable memory cannot shrink) and is reused by the next `begin`.
pub(crate) fn release() {
    let mem = pool_memory();
    if mem.size() == 0 {
        return;
    }
    mem.write(0, &[0u8; POOL_HEADER_SIZE as usize]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(stored_len: usize, byte: u8, aux: u8) -> RebuildCandidate {
        RebuildCandidate {
            stored: vec![byte; stored_len],
            aux: [aux; 8],
        }
    }

    #[test]
    fn pool_capacity_for_rejects_unfittable_geometry() {
        // A tiny geometry hosts far more than nlist rows.
        assert!(pool_capacity_for(16, 2, 16, false).expect("tiny geometry") > 100);
        // The coarse-id reservation shrinks a two-level capacity but keeps it positive here.
        let flat_cap = pool_capacity_for(16, 2, 16, false).expect("flat cap");
        let two_level_cap = pool_capacity_for(16, 2, 16, true).expect("two-level cap");
        assert!(two_level_cap < flat_cap);
        // Centroids alone exceeding the budget are rejected.
        let huge_stride = (REGION_BYTES / 2) as u32;
        assert!(pool_capacity_for(16, 2, huge_stride, false).is_none());
        // A stride so wide that even nlist rows do not fit is rejected.
        assert!(pool_capacity_for(u32::MAX - 8, 1024, 16, false).is_none());
    }

    #[test]
    fn begin_open_append_and_release_roundtrip() {
        release();
        assert!(bound_index().is_none(), "fresh fixture has no binding");

        begin(7, 32, 4, 16, false, false).expect("begin");
        assert_eq!(bound_index(), Some(7));

        // Exact validation matrix on open.
        assert_eq!(
            open(9, 32, 4, 16, false).unwrap_err(),
            PoolValidationError::ForeignIndex { bound: 7 },
            "a foreign index must not read another index's pool"
        );
        assert_eq!(
            open(7, 64, 4, 16, false).unwrap_err(),
            PoolValidationError::GeometryMismatch,
            "stride drift must be rejected"
        );
        assert_eq!(
            open(7, 32, 4, 16, true).unwrap_err(),
            PoolValidationError::GeometryMismatch,
            "a two-level open must not match a flat binding"
        );

        // Append/read parity, streaming parity, bulk-load parity.
        let rows: Vec<RebuildCandidate> = (0..5u8).map(|i| row(32, i, i + 1)).collect();
        append_rows(7, 32, 4, 16, false, &rows).expect("append");
        append_rows(7, 32, 4, 16, false, &[]).expect("empty append is a no-op");
        let opened = open(7, 32, 4, 16, false).expect("open");
        assert_eq!(opened.pool_len, 5);
        assert_eq!(opened.assigned_len, 0, "a flat binding never assigns");
        for (i, expected) in rows.iter().enumerate() {
            assert_eq!(&read_row(i as u32, 32), expected);
        }
        let loaded = load_rows(7, 32, 4, 16, false).expect("load");
        assert_eq!(loaded, rows);

        let mut streamed = Vec::new();
        for_each_row(32, |index, stored, aux| {
            streamed.push((index, stored.to_vec(), aux));
        })
        .expect("stream");
        assert_eq!(
            streamed,
            rows.iter()
                .enumerate()
                .map(|(i, r)| (i as u32, r.stored.clone(), r.aux))
                .collect::<Vec<_>>(),
            "the chunked stream must reproduce every durable row in order"
        );

        // reset_pool_rows keeps the binding but empties the candidate array.
        reset_pool_rows(7, 32, 4, 16, false).expect("reset rows");
        assert_eq!(open(7, 32, 4, 16, false).expect("open").pool_len, 0);

        // Release makes the region absent again (idempotent).
        release();
        release();
        assert!(bound_index().is_none());
        assert_eq!(
            open(7, 32, 4, 16, false).unwrap_err(),
            PoolValidationError::Absent
        );
    }

    #[test]
    fn two_level_coarse_ids_roundtrip_and_validate() {
        release();
        begin(31, 32, 4, 12, true, false).expect("begin two-level");

        let rows: Vec<RebuildCandidate> = (0..6u8).map(|i| row(32, i, i + 1)).collect();
        append_rows(31, 32, 4, 12, true, &rows).expect("append");

        // Coarse ids are rejected before they cover the whole pool.
        put_coarse_ids(31, 32, 4, 12, &[0, 1]).expect_err("length mismatch");
        put_coarse_ids(31, 32, 4, 12, &[0, 1, 2, 3, 4, 9]).expect_err("id out of range");
        assert_eq!(open(31, 32, 4, 12, true).expect("open").assigned_len, 0);

        let ids = [3u32, 0, 3, 1, 2, 0];
        put_coarse_ids(31, 32, 4, 12, &ids).expect("assign");
        assert_eq!(open(31, 32, 4, 12, true).expect("open").assigned_len, 6);

        let mut seen: Vec<(u32, u32)> = Vec::new();
        for_each_coarse_id(|index, id| seen.push((index, id))).expect("stream ids");
        assert_eq!(seen, (0..6u32).zip(ids).collect::<Vec<_>>());

        // A further append after assignment is rejected (sampling never runs past it).
        assert!(append_rows(31, 32, 4, 12, true, &[row(32, 9, 9)]).is_err());

        // reset_pool_rows drops the assignment with the rows it described: the durable length
        // returns to zero (so the next TrainFine message re-runs the assignment pass) and the
        // id stream rejects the emptied pool instead of streaming nothing.
        reset_pool_rows(31, 32, 4, 12, true).expect("reset rows");
        let opened = open(31, 32, 4, 12, true).expect("open");
        assert_eq!((opened.pool_len, opened.assigned_len), (0, 0));
        assert!(for_each_coarse_id(|_, _| {}).is_err(), "assignment gone");
        release();

        // A flat binding rejects the coarse-id APIs outright.
        begin(33, 32, 4, 12, false, false).expect("begin flat");
        assert!(put_coarse_ids(33, 32, 4, 12, &[]).is_err());
        assert!(for_each_coarse_id(|_, _| {}).is_err());
        release();
    }

    #[test]
    fn append_beyond_capacity_fails_closed_without_partial_visibility() {
        release();
        // A deliberately narrow capacity: ~500 KB rows leave room for only 16 slots under the
        // region budget (nlist=2, centroids 8 B).
        let pad_stride = 500_000u32;
        let capacity =
            pool_capacity_for(pad_stride, 2, 8, false).expect("geometry must fit") as usize;
        begin(11, pad_stride, 2, 8, false, false).expect("begin");
        let rows: Vec<RebuildCandidate> = (0..capacity as u8)
            .map(|i| row(pad_stride as usize, i, 0))
            .collect();
        append_rows(11, pad_stride, 2, 8, false, &rows).expect("fill to capacity");
        assert_eq!(
            open(11, pad_stride, 2, 8, false).expect("open").pool_len as usize,
            capacity
        );

        // One more row exceeds the physical array: rejected before any write lands.
        let overflow = vec![row(pad_stride as usize, 0xFF, 0)];
        assert!(
            append_rows(11, pad_stride, 2, 8, false, &overflow).is_err(),
            "append beyond capacity must fail closed"
        );
        assert_eq!(
            open(11, pad_stride, 2, 8, false).expect("open").pool_len as usize,
            capacity,
            "a failed append must not advance the durable length"
        );

        // A wrong-width row is rejected without mutating anything.
        append_rows(
            11,
            pad_stride - 1,
            2,
            8,
            false,
            &[row(pad_stride as usize, 1, 0)],
        )
        .expect_err("width mismatch");
        assert_eq!(
            open(11, pad_stride, 2, 8, false).expect("open").pool_len as usize,
            capacity
        );
    }

    #[test]
    fn centroid_area_roundtrip_and_validation() {
        release();
        begin(13, 16, 3, 12, false, false).expect("begin");
        assert!(
            get_centroids(13, 16, 3, 12, false).expect("get").is_empty(),
            "a fresh area reports no seeded centroids"
        );
        let centroids: Vec<Vec<u8>> = (0..3u8).map(|p| vec![p + 1; 12]).collect();
        put_centroids(13, 16, 3, 12, false, &centroids).expect("seed");
        assert_eq!(get_centroids(13, 16, 3, 12, false).expect("get"), centroids);
        // Overwriting a complete set is allowed (per-iteration refinement).
        let refined: Vec<Vec<u8>> = (0..3u8).map(|p| vec![p + 9; 12]).collect();
        put_centroids(13, 16, 3, 12, false, &refined).expect("refine");
        assert_eq!(get_centroids(13, 16, 3, 12, false).expect("get"), refined);

        // Wrong count or width fails closed.
        put_centroids(13, 16, 3, 12, false, &centroids[..2]).expect_err("count drift");
        put_centroids(13, 16, 3, 12, false, &vec![vec![0u8; 11]; 3]).expect_err("width mismatch");

        // After an explicit reset a different-length set may be seeded (coarse -> fine handover).
        reset_centroids(13, 16, 3, 12, false).expect("reset");
        let fine: Vec<Vec<u8>> = (0..2u8).map(|p| vec![p + 5; 12]).collect();
        put_centroids(13, 16, 3, 12, false, &fine).expect("reseed smaller");
        assert_eq!(get_centroids(13, 16, 3, 12, false).expect("get"), fine);
    }

    #[test]
    fn corrupt_header_variants_are_distinguished() {
        release();
        assert_eq!(
            open(1, 16, 2, 8, false).unwrap_err(),
            PoolValidationError::Absent
        );
        begin(21, 16, 2, 8, false, false).expect("begin");

        let saved = {
            let mem = pool_memory();
            let mut buf = [0u8; POOL_HEADER_SIZE as usize];
            mem.read(0, &mut buf);
            buf
        };

        // Magic corruption and version drift.
        let mem = pool_memory();
        mem.write(0, b"XXX");
        assert_eq!(
            open(21, 16, 2, 8, false).unwrap_err(),
            PoolValidationError::CorruptHeader
        );
        mem.write(0, &saved);
        mem.write(3, &[1]);
        assert_eq!(
            open(21, 16, 2, 8, false).unwrap_err(),
            PoolValidationError::CorruptHeader,
            "a version-1 header cannot be interpreted by this layout"
        );
        mem.write(0, &saved);

        // Tampered recorded capacity must disagree with the recomputed one.
        let mut tampered = saved;
        tampered[20..28].copy_from_slice(&(999u64.to_le_bytes()));
        mem.write(0, &tampered);
        assert_eq!(
            open(21, 16, 2, 8, false).unwrap_err(),
            PoolValidationError::GeometryMismatch
        );
        mem.write(0, &saved);

        // An assigned_len on a flat binding, or beyond pool_len, is corrupt.
        let mut assigned = saved;
        assigned[40..44].copy_from_slice(&1u32.to_le_bytes());
        mem.write(0, &assigned);
        assert_eq!(
            open(21, 16, 2, 8, false).unwrap_err(),
            PoolValidationError::LengthMismatch
        );
        mem.write(0, &saved);

        // A length beyond the capacity is corrupt even with valid geometry fields.
        let mut overlong = saved;
        overlong[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        mem.write(0, &overlong);
        assert_eq!(
            open(21, 16, 2, 8, false).unwrap_err(),
            PoolValidationError::LengthMismatch
        );
        mem.write(0, &saved);
    }
}
