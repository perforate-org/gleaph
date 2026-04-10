//! PocketIC / `canbench` harness for DGAP remove-slab, deleted-strategy comparison,
//! and segment-maintenance thresholds.

#![cfg_attr(target_arch = "wasm32", no_main)]

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::hint::black_box;

use canbench_rs::bench;
use ic_stable_csr::{
    Bound, CsrGraphWithGcQueueDenseDeleted, CsrGraphWithGcQueueRowTombstone,
    CsrGraphWithGcQueueSparseDeleted, DgapEdgeStore, DgapGraphMemories, DgapStores,
    SegmentEdgeCounts, SegmentMaintainAction, SegmentMaintainThresholds, Storable,
    dgap::{RebalanceDecision, segment_maintenance_decision},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex, CsrVertexTombstone},
};
use ic_stable_slot_map::SlotMap;
use ic_stable_structures::DefaultMemoryImpl;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};

mod wipe;

type BenchMemory = VirtualMemory<DefaultMemoryImpl>;

const DEG_TOMB: u32 = 1u32 << 31;
const BASE_SEGMENT_SIZE: u32 = 128;
const COMMUNITY_SIZE: usize = 64;
const RAW_READ_PROBE_COUNT: usize = 64;
const WORKLOAD_STEPS: usize = 100;
const RANDOM_SPARSE_DEGREE: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchVertex {
    slot_base: u64,
    deg: u32,
    log_head: i32,
}

impl CsrVertex for BenchVertex {
    fn base_slot_start(&self) -> u64 {
        self.slot_base
    }
    fn degree(&self) -> u32 {
        self.deg & !DEG_TOMB
    }
    fn with_base_slot_start(self, start: u64) -> Self {
        Self {
            slot_base: start,
            ..self
        }
    }
    fn with_degree(self, degree: u32) -> Self {
        Self {
            deg: (self.deg & DEG_TOMB) | (degree & !DEG_TOMB),
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

impl CsrVertexTombstone for BenchVertex {
    fn is_tombstone(&self) -> bool {
        (self.deg & DEG_TOMB) != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        Self {
            deg: if tombstone {
                self.deg | DEG_TOMB
            } else {
                self.deg & !DEG_TOMB
            },
            ..self
        }
    }
}

impl Storable for BenchVertex {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BenchEdge([u8; 4]);

impl CsrEdge for BenchEdge {
    const EDGE_BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(bytes.try_into().unwrap())
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.0);
    }

    fn neighbor_vid(&self) -> usize {
        u32::from_le_bytes(self.0) as usize
    }

    fn with_neighbor_vid(self, vid: usize) -> Self {
        Self((vid as u32).to_le_bytes())
    }
}

impl CsrEdgeTombstone for BenchEdge {
    fn is_tombstone(&self) -> bool {
        self.0[2] != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        let mut b = self.0;
        b[2] = if tombstone { 1 } else { 0 };
        Self(b)
    }
}

type BenchEdgeStore = DgapEdgeStore<BenchEdge, BenchMemory, BenchMemory>;
type BenchStores = DgapStores<BenchVertex, BenchEdge, BenchMemory, BenchMemory, BenchMemory>;
type BenchRowGraph = CsrGraphWithGcQueueRowTombstone<
    BenchVertex,
    BenchEdge,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
>;
type BenchSparseGraph = CsrGraphWithGcQueueSparseDeleted<
    BenchVertex,
    BenchEdge,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
>;
type BenchDenseGraph = CsrGraphWithGcQueueDenseDeleted<
    BenchVertex,
    BenchEdge,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BenchVariant {
    Row,
    Sparse,
    Dense,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphTopology {
    Chain,
    HubStar,
    UniformRandomSparse,
    PowerLaw,
    ClusteredCommunity,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeletePattern {
    UniformRandom,
    ClusteredContiguous,
    HubFirst,
    LeafFirst,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteDensity {
    D001Pct,
    D1Pct,
    D10Pct,
    D50Pct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkloadKind {
    ReadHeavy,
    Mixed,
    DeleteHeavy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GraphFormat {
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct WorkloadSummary {
    deleted_count: usize,
    yielded_edge_count: usize,
    queue_len_before: u64,
    queue_len_after: u64,
    completed_gc_items: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FixtureSpec {
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    seed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FixtureImage {
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn gen_range(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            return 0;
        }
        (self.next_u64() % upper as u64) as usize
    }
}

fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut rng = XorShift64::new(seed);
    for idx in (1..items.len()).rev() {
        let swap_idx = rng.gen_range(idx + 1);
        items.swap(idx, swap_idx);
    }
}

fn empty_vertex() -> BenchVertex {
    BenchVertex {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }
}

fn graph_format_for_vertices(n: usize) -> GraphFormat {
    let segment_size = BASE_SEGMENT_SIZE;
    let segment_count = ((n.max(1).div_ceil(segment_size as usize)).next_power_of_two())
        .max(1)
        .min(u32::MAX as usize) as u32;
    let elem_capacity = (n.max(1) as u64)
        .saturating_mul(8)
        .max(65_536)
        .next_power_of_two();
    GraphFormat {
        elem_capacity,
        segment_count,
        segment_size,
    }
}

fn delete_count(n: usize, density: DeleteDensity) -> usize {
    let numerator = match density {
        DeleteDensity::D001Pct => 1usize,
        DeleteDensity::D1Pct => 10usize,
        DeleteDensity::D10Pct => 100usize,
        DeleteDensity::D50Pct => 500usize,
    };
    let denom = 1000usize;
    ((n.saturating_mul(numerator)).max(denom) / denom).clamp(1, n.max(1))
}

fn generate_edges(topology: GraphTopology, n: usize, seed: u64) -> Vec<(usize, usize)> {
    match topology {
        GraphTopology::Chain => generate_chain_edges(n),
        GraphTopology::HubStar => generate_hub_star_edges(n),
        GraphTopology::UniformRandomSparse => generate_uniform_random_sparse_edges(n, seed),
        GraphTopology::PowerLaw => generate_power_law_edges(n),
        GraphTopology::ClusteredCommunity => generate_clustered_community_edges(n),
    }
}

fn generate_chain_edges(n: usize) -> Vec<(usize, usize)> {
    (0..n.saturating_sub(1)).map(|i| (i, i + 1)).collect()
}

fn generate_hub_star_edges(n: usize) -> Vec<(usize, usize)> {
    let mut edges = Vec::with_capacity(n.saturating_sub(1) * 2);
    for i in 1..n {
        edges.push((0, i));
        edges.push((i, 0));
    }
    edges
}

fn generate_uniform_random_sparse_edges(n: usize, seed: u64) -> Vec<(usize, usize)> {
    let mut rng = XorShift64::new(seed);
    let mut edges = Vec::with_capacity(n.saturating_mul(RANDOM_SPARSE_DEGREE));
    for src in 0..n {
        let mut picked = BTreeSet::new();
        while picked.len() < RANDOM_SPARSE_DEGREE.min(n.saturating_sub(1)) {
            let dst = rng.gen_range(n);
            if dst != src {
                picked.insert(dst);
            }
        }
        for dst in picked {
            edges.push((src, dst));
        }
    }
    edges
}

fn generate_power_law_edges(n: usize) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    for src in 1..n {
        edges.push((src - 1, src));
        edges.push((src, 0));
        if src % 2 == 0 && n > 2 {
            edges.push((src, 1));
        }
        if src % 4 == 0 && n > 4 {
            edges.push((src, 3));
        }
        if src % 8 == 0 && n > 8 {
            edges.push((src, 7));
        }
    }
    dedup_edges(edges)
}

fn generate_clustered_community_edges(n: usize) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    for base in (0..n).step_by(COMMUNITY_SIZE) {
        let end = (base + COMMUNITY_SIZE).min(n);
        for src in base..end {
            for step in 1..=3 {
                let dst = src + step;
                if dst < end {
                    edges.push((src, dst));
                }
            }
        }
        let next = end;
        if next < n {
            edges.push((base, next));
            edges.push((next, base));
        }
    }
    dedup_edges(edges)
}

fn dedup_edges(edges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::with_capacity(edges.len());
    for edge in edges {
        if edge.0 != edge.1 && seen.insert(edge) {
            deduped.push(edge);
        }
    }
    deduped
}

fn make_delete_set(
    pattern: DeletePattern,
    n: usize,
    density: DeleteDensity,
    seed: u64,
) -> Vec<usize> {
    let count = delete_count(n, density);
    match pattern {
        DeletePattern::UniformRandom => {
            let mut rng = XorShift64::new(seed ^ 0xA5A5_5A5A_A5A5_5A5A);
            let mut picked = BTreeSet::new();
            while picked.len() < count {
                picked.insert(rng.gen_range(n));
            }
            picked.into_iter().collect()
        }
        DeletePattern::ClusteredContiguous => {
            let mut rng = XorShift64::new(seed ^ 0xDEAD_BEEF_CAFE_BABE);
            let start = rng.gen_range(n);
            (0..count).map(|i| (start + i) % n).collect()
        }
        DeletePattern::HubFirst => (0..count).collect(),
        DeletePattern::LeafFirst => ((n - count)..n).collect(),
    }
}

fn choose_probe_vertices(n: usize, deleted: &[usize]) -> Vec<usize> {
    let deleted: BTreeSet<_> = deleted.iter().copied().collect();
    let mut probes = Vec::new();
    for vid in 0..n {
        if !deleted.contains(&vid) {
            probes.push(vid);
        }
        if probes.len() == RAW_READ_PROBE_COUNT {
            break;
        }
    }
    if probes.is_empty() {
        probes.push(0);
    }
    probes
}

trait QueueGraphOps {
    fn delete_edge_directed(&self, src: usize, dst: usize);
    fn delete_vertex(&self, vid: usize);
    fn gc_step(&self, budget: usize) -> usize;
    fn work_queue_len(&self) -> u64;
    fn raw_out_edge_count(&self, vid: usize) -> usize;
    fn logical_out_edge_count(&self, vid: usize) -> Option<usize>;
}

impl QueueGraphOps for BenchRowGraph {
    fn delete_edge_directed(&self, src: usize, dst: usize) {
        self.delete_edge_directed(src, dst)
            .expect("delete_edge_directed");
    }
    fn delete_vertex(&self, vid: usize) {
        self.delete_vertex(vid).expect("delete_vertex");
    }
    fn gc_step(&self, budget: usize) -> usize {
        self.gc_step(budget).expect("gc_step")
    }
    fn work_queue_len(&self) -> u64 {
        self.work_queue_len()
    }
    fn raw_out_edge_count(&self, vid: usize) -> usize {
        self.graph().out_edges(vid).expect("out_edges").count()
    }
    fn logical_out_edge_count(&self, _vid: usize) -> Option<usize> {
        None
    }
}

impl QueueGraphOps for BenchSparseGraph {
    fn delete_edge_directed(&self, src: usize, dst: usize) {
        self.delete_edge_directed(src, dst)
            .expect("delete_edge_directed");
    }
    fn delete_vertex(&self, vid: usize) {
        self.delete_vertex(vid).expect("delete_vertex");
    }
    fn gc_step(&self, budget: usize) -> usize {
        self.gc_step(budget).expect("gc_step")
    }
    fn work_queue_len(&self) -> u64 {
        self.work_queue_len()
    }
    fn raw_out_edge_count(&self, vid: usize) -> usize {
        self.graph().out_edges(vid).expect("out_edges").count()
    }
    fn logical_out_edge_count(&self, vid: usize) -> Option<usize> {
        Some(
            self.out_edges_logical(vid)
                .expect("out_edges_logical")
                .count(),
        )
    }
}

impl QueueGraphOps for BenchDenseGraph {
    fn delete_edge_directed(&self, src: usize, dst: usize) {
        self.delete_edge_directed(src, dst)
            .expect("delete_edge_directed");
    }
    fn delete_vertex(&self, vid: usize) {
        self.delete_vertex(vid).expect("delete_vertex");
    }
    fn gc_step(&self, budget: usize) -> usize {
        self.gc_step(budget).expect("gc_step")
    }
    fn work_queue_len(&self) -> u64 {
        self.work_queue_len()
    }
    fn raw_out_edge_count(&self, vid: usize) -> usize {
        self.graph().out_edges(vid).expect("out_edges").count()
    }
    fn logical_out_edge_count(&self, vid: usize) -> Option<usize> {
        Some(
            self.out_edges_logical(vid)
                .expect("out_edges_logical")
                .count(),
        )
    }
}

enum BenchVariantGraph {
    Row(BenchRowGraph),
    Sparse(BenchSparseGraph),
    Dense(BenchDenseGraph),
}

impl BenchVariantGraph {
    fn append_empty_vertices_fast_for_fixture(&self, row: BenchVertex, count: usize) {
        match self {
            Self::Row(g) => g
                .append_empty_vertices_fast_for_fixture(row, count)
                .expect("append_empty_vertices_fast_for_fixture"),
            Self::Sparse(g) => g
                .append_empty_vertices_fast_for_fixture(row, count)
                .expect("append_empty_vertices_fast_for_fixture"),
            Self::Dense(g) => g
                .append_empty_vertices_fast_for_fixture(row, count)
                .expect("append_empty_vertices_fast_for_fixture"),
        }
    }

    fn delete_edge_directed(&self, src: usize, dst: usize) {
        match self {
            Self::Row(g) => QueueGraphOps::delete_edge_directed(g, src, dst),
            Self::Sparse(g) => QueueGraphOps::delete_edge_directed(g, src, dst),
            Self::Dense(g) => QueueGraphOps::delete_edge_directed(g, src, dst),
        }
    }

    fn delete_vertex(&self, vid: usize) {
        match self {
            Self::Row(g) => QueueGraphOps::delete_vertex(g, vid),
            Self::Sparse(g) => QueueGraphOps::delete_vertex(g, vid),
            Self::Dense(g) => QueueGraphOps::delete_vertex(g, vid),
        }
    }

    fn gc_step(&self, budget: usize) -> usize {
        match self {
            Self::Row(g) => QueueGraphOps::gc_step(g, budget),
            Self::Sparse(g) => QueueGraphOps::gc_step(g, budget),
            Self::Dense(g) => QueueGraphOps::gc_step(g, budget),
        }
    }

    fn work_queue_len(&self) -> u64 {
        match self {
            Self::Row(g) => QueueGraphOps::work_queue_len(g),
            Self::Sparse(g) => QueueGraphOps::work_queue_len(g),
            Self::Dense(g) => QueueGraphOps::work_queue_len(g),
        }
    }

    fn raw_out_edge_count(&self, vid: usize) -> usize {
        match self {
            Self::Row(g) => QueueGraphOps::raw_out_edge_count(g, vid),
            Self::Sparse(g) => QueueGraphOps::raw_out_edge_count(g, vid),
            Self::Dense(g) => QueueGraphOps::raw_out_edge_count(g, vid),
        }
    }

    fn logical_out_edge_count(&self, vid: usize) -> Option<usize> {
        match self {
            Self::Row(g) => QueueGraphOps::logical_out_edge_count(g, vid),
            Self::Sparse(g) => QueueGraphOps::logical_out_edge_count(g, vid),
            Self::Dense(g) => QueueGraphOps::logical_out_edge_count(g, vid),
        }
    }

    fn raw_out_neighbors(&self, vid: usize) -> Vec<usize> {
        match self {
            Self::Row(g) => g
                .graph()
                .out_edges(vid)
                .expect("out_edges")
                .map(|r| r.expect("raw edge").neighbor_vid())
                .collect(),
            Self::Sparse(g) => g
                .graph()
                .out_edges(vid)
                .expect("out_edges")
                .map(|r| r.expect("raw edge").neighbor_vid())
                .collect(),
            Self::Dense(g) => g
                .graph()
                .out_edges(vid)
                .expect("out_edges")
                .map(|r| r.expect("raw edge").neighbor_vid())
                .collect(),
        }
    }

    fn raw_in_has_neighbor(&self, vid: usize, neighbor: usize) -> bool {
        match self {
            Self::Row(g) => g
                .graph()
                .in_edges(vid)
                .expect("in_edges")
                .any(|r| r.expect("raw edge").neighbor_vid() == neighbor),
            Self::Sparse(g) => g
                .graph()
                .in_edges(vid)
                .expect("in_edges")
                .any(|r| r.expect("raw edge").neighbor_vid() == neighbor),
            Self::Dense(g) => g
                .graph()
                .in_edges(vid)
                .expect("in_edges")
                .any(|r| r.expect("raw edge").neighbor_vid() == neighbor),
        }
    }

    fn has_raw_edge_pair(&self, src: usize, dst: usize) -> bool {
        self.raw_out_neighbors(src).into_iter().any(|n| n == dst)
            && self.raw_in_has_neighbor(dst, src)
    }

    fn forward_dgap(&self) -> &BenchStores {
        match self {
            Self::Row(g) => g.graph().forward_dgap(),
            Self::Sparse(g) => g.graph().forward_dgap(),
            Self::Dense(g) => g.graph().forward_dgap(),
        }
    }

    fn reverse_dgap(&self) -> &BenchStores {
        match self {
            Self::Row(g) => g.graph().reverse_dgap(),
            Self::Sparse(g) => g.graph().reverse_dgap(),
            Self::Dense(g) => g.graph().reverse_dgap(),
        }
    }
}

fn format_queue_graph(variant: BenchVariant, format: GraphFormat) -> BenchVariantGraph {
    let mgr = MemoryManager::init(DefaultMemoryImpl::default());
    match variant {
        BenchVariant::Row => {
            let g = BenchRowGraph::format_new_with_gc_queue(
                mgr.get(MemoryId::new(0)),
                mgr.get(MemoryId::new(1)),
                mgr.get(MemoryId::new(2)),
                mgr.get(MemoryId::new(3)),
                mgr.get(MemoryId::new(4)),
                mgr.get(MemoryId::new(5)),
                mgr.get(MemoryId::new(6)),
                format.elem_capacity,
                format.segment_count,
                format.segment_size,
                0,
                None,
            )
            .expect("format row");
            BenchVariantGraph::Row(g)
        }
        BenchVariant::Sparse => {
            let g = BenchSparseGraph::format_new_with_gc_queue(
                mgr.get(MemoryId::new(0)),
                mgr.get(MemoryId::new(1)),
                mgr.get(MemoryId::new(2)),
                mgr.get(MemoryId::new(3)),
                mgr.get(MemoryId::new(4)),
                mgr.get(MemoryId::new(5)),
                mgr.get(MemoryId::new(6)),
                mgr.get(MemoryId::new(7)),
                format.elem_capacity,
                format.segment_count,
                format.segment_size,
                0,
                None,
            )
            .expect("format sparse");
            BenchVariantGraph::Sparse(g)
        }
        BenchVariant::Dense => {
            let g = BenchDenseGraph::format_new_with_gc_queue(
                mgr.get(MemoryId::new(0)),
                mgr.get(MemoryId::new(1)),
                mgr.get(MemoryId::new(2)),
                mgr.get(MemoryId::new(3)),
                mgr.get(MemoryId::new(4)),
                mgr.get(MemoryId::new(5)),
                mgr.get(MemoryId::new(6)),
                mgr.get(MemoryId::new(7)),
                format.elem_capacity,
                format.segment_count,
                format.segment_size,
                0,
                None,
            )
            .expect("format dense");
            BenchVariantGraph::Dense(g)
        }
    }
}

fn open_queue_graph(variant: BenchVariant) -> BenchVariantGraph {
    let mgr = MemoryManager::init(DefaultMemoryImpl::default());
    match variant {
        BenchVariant::Row => {
            let g = BenchRowGraph::open_existing_with_gc_queue(
                mgr.get(MemoryId::new(0)),
                mgr.get(MemoryId::new(1)),
                mgr.get(MemoryId::new(2)),
                mgr.get(MemoryId::new(3)),
                mgr.get(MemoryId::new(4)),
                mgr.get(MemoryId::new(5)),
                mgr.get(MemoryId::new(6)),
                None,
            )
            .expect("open row");
            BenchVariantGraph::Row(g)
        }
        BenchVariant::Sparse => {
            let g = BenchSparseGraph::open_existing_with_gc_queue(
                mgr.get(MemoryId::new(0)),
                mgr.get(MemoryId::new(1)),
                mgr.get(MemoryId::new(2)),
                mgr.get(MemoryId::new(3)),
                mgr.get(MemoryId::new(4)),
                mgr.get(MemoryId::new(5)),
                mgr.get(MemoryId::new(6)),
                mgr.get(MemoryId::new(7)),
                None,
            )
            .expect("open sparse");
            BenchVariantGraph::Sparse(g)
        }
        BenchVariant::Dense => {
            let g = BenchDenseGraph::open_existing_with_gc_queue(
                mgr.get(MemoryId::new(0)),
                mgr.get(MemoryId::new(1)),
                mgr.get(MemoryId::new(2)),
                mgr.get(MemoryId::new(3)),
                mgr.get(MemoryId::new(4)),
                mgr.get(MemoryId::new(5)),
                mgr.get(MemoryId::new(6)),
                mgr.get(MemoryId::new(7)),
                None,
            )
            .expect("open dense");
            BenchVariantGraph::Dense(g)
        }
    }
}

fn populate_vertices_bulk(graph: &BenchVariantGraph, n: usize) {
    graph.append_empty_vertices_fast_for_fixture(empty_vertex(), n);
}

fn build_packed_edge_column(
    stores: &BenchStores,
    n: usize,
    edges: &[(usize, usize)],
    reverse: bool,
) {
    let h = stores.edges.header().expect("edge header");
    let stride = h.edge_stride as usize;

    let mut owner_offsets = vec![0usize; n + 1];
    for &(src, dst) in edges {
        let owner = if reverse { dst } else { src };
        owner_offsets[owner + 1] += 1;
    }
    for owner in 0..n {
        owner_offsets[owner + 1] += owner_offsets[owner];
    }

    let total_edges = owner_offsets[n];
    let mut neighbors_flat = vec![0usize; total_edges];
    let mut owner_cursor = owner_offsets[..n].to_vec();
    for &(src, dst) in edges {
        let (owner, neighbor) = if reverse { (dst, src) } else { (src, dst) };
        let slot = owner_cursor[owner];
        neighbors_flat[slot] = neighbor;
        owner_cursor[owner] += 1;
    }

    for owner in 0..n {
        neighbors_flat[owner_offsets[owner]..owner_offsets[owner + 1]].sort_unstable();
    }

    let mut payload = vec![0u8; total_edges * stride];
    for (i, neighbor) in neighbors_flat.into_iter().enumerate() {
        BenchEdge::default()
            .with_neighbor_vid(neighbor)
            .write_to(&mut payload[i * stride..(i + 1) * stride]);
    }
    if !payload.is_empty() {
        stores
            .edges
            .memories()
            .write_edge_slab_span(h.edge_stride, 0, &payload)
            .expect("write packed slab span");
    }

    for owner in 0..n {
        let base = owner_offsets[owner] as u64;
        let degree = owner_offsets[owner + 1] - owner_offsets[owner];
        let prev = stores
            .vertices
            .get_dense(owner as u32)
            .expect("vertex row for packed edge build");
        let row = prev
            .with_base_slot_start(base)
            .with_degree(degree as u32)
            .with_log_head(-1)
            .with_tombstone(false);
        stores
            .vertices
            .set_dense(owner as u32, &row)
            .expect("packed edge build row update");
    }
    stores.edges.set_num_edges_header(total_edges as u64);
    stores
        .refresh_slab_occupied_tail_meta()
        .expect("refresh_slab_occupied_tail_meta");
    stores.sync_pma_meta().expect("sync_pma_meta");
}

fn bulk_load_fixture_graph(graph: &BenchVariantGraph, n: usize, edges: &[(usize, usize)]) {
    populate_vertices_bulk(graph, n);
    build_packed_edge_column(graph.forward_dgap(), n, edges, false);
    build_packed_edge_column(graph.reverse_dgap(), n, edges, true);
}

fn build_variant_graph(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    seed: u64,
) -> BenchVariantGraph {
    let edges = generate_edges(topology, n, seed);
    let format = graph_format_for_vertices(n);
    let graph = format_queue_graph(variant, format);
    bulk_load_fixture_graph(&graph, n, &edges);
    graph
}

fn build_vertices_only_graph(variant: BenchVariant, n: usize) -> BenchVariantGraph {
    let format = graph_format_for_vertices(n);
    let graph = format_queue_graph(variant, format);
    populate_vertices_bulk(&graph, n);
    graph
}

fn build_fixture(spec: FixtureSpec) -> FixtureImage {
    wipe::wipe_stable_memory();
    let _graph = build_variant_graph(spec.variant, spec.topology, spec.n, spec.seed);
    FixtureImage {
        bytes: wipe::snapshot_stable_memory(),
    }
}

fn build_vertices_fixture(spec: FixtureSpec) -> FixtureImage {
    wipe::wipe_stable_memory();
    let _graph = build_vertices_only_graph(spec.variant, spec.n);
    FixtureImage {
        bytes: wipe::snapshot_stable_memory(),
    }
}

fn load_fixture(spec: FixtureSpec, image: &FixtureImage) -> BenchVariantGraph {
    let _ = spec;
    wipe::restore_stable_memory(&image.bytes);
    open_queue_graph(spec.variant)
}

fn build_edges_from_fixture(graph: &BenchVariantGraph, topology: GraphTopology, n: usize, seed: u64) {
    let edges = generate_edges(topology, n, seed);
    build_packed_edge_column(graph.forward_dgap(), n, &edges, false);
    build_packed_edge_column(graph.reverse_dgap(), n, &edges, true);
}

fn apply_delete_set(graph: &BenchVariantGraph, deleted: &[usize]) -> usize {
    for &vid in deleted {
        graph.delete_vertex(vid);
    }
    deleted.len()
}

fn collect_actual_edges(graph: &BenchVariantGraph, n: usize) -> Vec<(usize, usize)> {
    let mut edges = Vec::new();
    for src in 0..n {
        edges.extend(
            graph
                .raw_out_neighbors(src)
                .into_iter()
                .map(|dst| (src, dst)),
        );
    }
    edges
}

fn order_edge_candidates(edges: &mut [(usize, usize)], delete_pattern: DeletePattern, seed: u64) {
    match delete_pattern {
        DeletePattern::UniformRandom => shuffle(edges, seed ^ 0xEE11_AADD),
        DeletePattern::ClusteredContiguous => {}
        DeletePattern::HubFirst => edges.sort_unstable_by_key(|&(src, dst)| src.min(dst)),
        DeletePattern::LeafFirst => {
            edges.sort_unstable_by_key(|&(src, dst)| std::cmp::Reverse(src.max(dst)))
        }
    }
}

fn pick_unique_edge_delete_set(
    edges: impl IntoIterator<Item = (usize, usize)>,
    target: usize,
) -> Vec<(usize, usize)> {
    let mut selected = Vec::with_capacity(target);
    let mut used_src = BTreeSet::new();
    let mut used_dst = BTreeSet::new();
    for (src, dst) in edges {
        if used_src.contains(&src) || used_dst.contains(&dst) {
            continue;
        }
        used_src.insert(src);
        used_dst.insert(dst);
        selected.push((src, dst));
        if selected.len() == target {
            break;
        }
    }
    selected
}

fn make_edge_delete_set_for_graph(
    graph: &BenchVariantGraph,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> Vec<(usize, usize)> {
    let target = delete_count(n, density);
    let mut generated = generate_edges(topology, n, seed);
    order_edge_candidates(&mut generated, delete_pattern, seed);

    let mut selected = pick_unique_edge_delete_set(
        generated
            .into_iter()
            .filter(|&(src, dst)| graph.has_raw_edge_pair(src, dst)),
        target,
    );
    if selected.len() == target {
        return selected;
    }

    let mut actual = collect_actual_edges(graph, n);
    order_edge_candidates(&mut actual, delete_pattern, seed ^ 0x91C2_44EF);

    let mut used_src: BTreeSet<_> = selected.iter().map(|&(src, _)| src).collect();
    let mut used_dst: BTreeSet<_> = selected.iter().map(|&(_, dst)| dst).collect();
    for (src, dst) in actual {
        if used_src.contains(&src) || used_dst.contains(&dst) {
            continue;
        }
        used_src.insert(src);
        used_dst.insert(dst);
        selected.push((src, dst));
        if selected.len() == target {
            break;
        }
    }
    selected
}

fn run_gc_to_completion(graph: &BenchVariantGraph, budget: usize) -> usize {
    let mut completed = 0usize;
    loop {
        if graph.work_queue_len() == 0 {
            break;
        }
        let _scope = canbench_rs::bench_scope("dgap_gc_step_run_leaf");
        let step_done = graph.gc_step(budget);
        drop(_scope);
        let _pop_scope = canbench_rs::bench_scope("dgap_gc_step_pop_queue");
        black_box(graph.work_queue_len());
        if step_done == 0 {
            break;
        }
        completed += step_done;
    }
    completed
}

fn scan_raw_reads(graph: &BenchVariantGraph, probe_vertices: &[usize]) -> usize {
    let _scope = canbench_rs::bench_scope("dgap_raw_read_scan");
    probe_vertices
        .iter()
        .map(|&vid| graph.raw_out_edge_count(vid))
        .sum()
}

fn scan_logical_reads(graph: &BenchVariantGraph, probe_vertices: &[usize]) -> usize {
    let _scan_scope = canbench_rs::bench_scope("dgap_logical_read_scan");
    let counts: Vec<_> = probe_vertices
        .iter()
        .map(|&vid| graph.logical_out_edge_count(vid).unwrap_or(0))
        .collect();
    drop(_scan_scope);
    let _filter_scope = canbench_rs::bench_scope("dgap_logical_read_deleted_filter");
    let yielded: usize = counts.iter().sum();
    drop(_filter_scope);
    let _yield_scope = canbench_rs::bench_scope("dgap_logical_read_yield");
    black_box(yielded);
    yielded
}

fn run_workload(
    graph: &BenchVariantGraph,
    kind: WorkloadKind,
    delete_set: &[usize],
    probe_vertices: &[usize],
) -> WorkloadSummary {
    let mut summary = WorkloadSummary {
        queue_len_before: graph.work_queue_len(),
        ..Default::default()
    };
    let mut delete_idx = 0usize;
    for step in 0..WORKLOAD_STEPS {
        let do_delete = match kind {
            WorkloadKind::ReadHeavy => step % 20 == 19,
            WorkloadKind::Mixed => step % 10 == 8,
            WorkloadKind::DeleteHeavy => step % 5 <= 1,
        };
        let do_gc = match kind {
            WorkloadKind::ReadHeavy => step % 25 == 24,
            WorkloadKind::Mixed => step % 10 == 9,
            WorkloadKind::DeleteHeavy => step % 5 == 4,
        };
        if do_delete && delete_idx < delete_set.len() {
            graph.delete_vertex(delete_set[delete_idx]);
            delete_idx += 1;
            summary.deleted_count += 1;
        } else {
            let vid = probe_vertices[step % probe_vertices.len()];
            summary.yielded_edge_count += graph
                .logical_out_edge_count(vid)
                .unwrap_or_else(|| graph.raw_out_edge_count(vid));
        }
        if do_gc {
            summary.completed_gc_items += run_gc_to_completion(graph, 32);
        }
    }
    summary.queue_len_after = graph.work_queue_len();
    summary
}

/// Measures the end-to-end cost of applying a preselected vertex delete set to a freshly
/// reopened fixture graph.
///
/// This benchmark excludes fixture construction time from the measured scope. It captures:
/// vertex tombstoning, deleted-index updates for sparse/dense variants, SEC refresh, and GC
/// work scheduling caused by `delete_vertex`.
fn bench_delete_vertices(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let fixture = build_fixture(spec);
    let graph = load_fixture(spec, &fixture);
    let delete_set = make_delete_set(delete_pattern, n, density, seed);
    canbench_rs::bench_fn(move || {
        let summary = WorkloadSummary {
            deleted_count: apply_delete_set(&graph, &delete_set),
            queue_len_before: 0,
            queue_len_after: graph.work_queue_len(),
            completed_gc_items: 0,
            yielded_edge_count: 0,
        };
        black_box(summary);
    })
}

/// Measures deferred cleanup after a delete phase by running `gc_step` until the queue drains.
///
/// The fixture is reopened, the delete set is applied outside the measured closure, and the
/// benchmark then times only the queue-driven maintenance path. This isolates the cost of
/// physical edge cleanup from the logical delete cost.
fn bench_gc_after_delete(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let fixture = build_fixture(spec);
    let graph = load_fixture(spec, &fixture);
    let delete_set = make_delete_set(delete_pattern, n, density, seed);
    apply_delete_set(&graph, &delete_set);
    let queue_before = graph.work_queue_len();
    canbench_rs::bench_fn(move || {
        let summary = WorkloadSummary {
            deleted_count: delete_set.len(),
            queue_len_before: queue_before,
            queue_len_after: graph.work_queue_len(),
            completed_gc_items: run_gc_to_completion(&graph, 32),
            yielded_edge_count: 0,
        };
        black_box(summary);
    })
}

/// Measures directed edge deletion on a reopened fixture.
///
/// The delete candidates are derived from edges that are actually present in the reopened graph,
/// so the benchmark reflects `delete_edge_directed` itself rather than fixture/topology drift.
/// The measured scope includes forward/reverse tombstoning, degree updates, SEC updates, and
/// any GC scheduling triggered by edge deletion.
fn bench_delete_edges(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let fixture = build_fixture(spec);
    let graph = load_fixture(spec, &fixture);
    let delete_edges =
        make_edge_delete_set_for_graph(&graph, topology, n, delete_pattern, density, seed);
    canbench_rs::bench_fn(move || {
        for &(src, dst) in &delete_edges {
            graph.delete_edge_directed(src, dst);
        }
        let summary = WorkloadSummary {
            deleted_count: delete_edges.len(),
            queue_len_before: 0,
            queue_len_after: graph.work_queue_len(),
            completed_gc_items: 0,
            yielded_edge_count: 0,
        };
        black_box(summary);
    })
}

/// Measures raw out-edge scans after logical deletions have already been applied.
///
/// This benchmark is primarily for comparing the low-level storage traversal cost across all
/// variants, including `RowTombstone`, which intentionally does not expose logical iteration.
fn bench_raw_read(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let fixture = build_fixture(spec);
    let graph = load_fixture(spec, &fixture);
    let delete_set = make_delete_set(delete_pattern, n, density, seed);
    apply_delete_set(&graph, &delete_set);
    let probes = choose_probe_vertices(n, &delete_set);
    canbench_rs::bench_fn(move || {
        let summary = WorkloadSummary {
            deleted_count: delete_set.len(),
            yielded_edge_count: scan_raw_reads(&graph, &probes),
            queue_len_before: graph.work_queue_len(),
            queue_len_after: graph.work_queue_len(),
            completed_gc_items: 0,
        };
        black_box(summary);
    })
}

/// Measures logical out-edge scans after a delete set has been applied.
///
/// Only sparse/dense variants use this path. The benchmark includes tombstone/deleted-neighbor
/// filtering cost and is meant to represent user-visible "read current graph view" performance.
fn bench_logical_read(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let fixture = build_fixture(spec);
    let graph = load_fixture(spec, &fixture);
    let delete_set = make_delete_set(delete_pattern, n, density, seed);
    apply_delete_set(&graph, &delete_set);
    let probes = choose_probe_vertices(n, &delete_set);
    canbench_rs::bench_fn(move || {
        let summary = WorkloadSummary {
            deleted_count: delete_set.len(),
            yielded_edge_count: scan_logical_reads(&graph, &probes),
            queue_len_before: graph.work_queue_len(),
            queue_len_after: graph.work_queue_len(),
            completed_gc_items: 0,
        };
        black_box(summary);
    })
}

/// Measures a mixed workload over a reopened fixture graph.
///
/// Depending on `kind`, the runner interleaves reads, vertex deletes, and periodic GC to model
/// service-like traffic rather than a single primitive operation.
fn bench_workload(
    variant: BenchVariant,
    kind: WorkloadKind,
    topology: GraphTopology,
    n: usize,
    delete_pattern: DeletePattern,
    density: DeleteDensity,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let fixture = build_fixture(spec);
    let graph = load_fixture(spec, &fixture);
    let delete_set = make_delete_set(delete_pattern, n, density, seed);
    let probes = choose_probe_vertices(n, &[]);
    canbench_rs::bench_fn(move || {
        let summary = run_workload(&graph, kind, &delete_set, &probes);
        black_box(summary);
    })
}

/// Measures fixture construction cost itself.
///
/// Unlike the operation benchmarks, this intentionally includes graph formatting, vertex
/// population, edge bulk load, and stable-memory snapshot generation so build cost can be
/// tracked separately from delete/read/GC costs.
fn bench_build_fixture(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    seed: u64,
) -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(move || {
        let image = build_fixture(FixtureSpec {
            variant,
            topology,
            n,
            seed,
        });
        black_box(image.bytes.len());
    })
}

/// Measures only the bulk vertex-append phase for fixture setup.
fn bench_build_vertices(variant: BenchVariant, n: usize) -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(move || {
        wipe::wipe_stable_memory();
        let graph = build_vertices_only_graph(variant, n);
        black_box(graph.forward_dgap().vertices.len());
    })
}

/// Measures only the packed edge-build phase, starting from a vertex-only fixture snapshot.
fn bench_build_edges(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    seed: u64,
) -> canbench_rs::BenchResult {
    let spec = FixtureSpec {
        variant,
        topology,
        n,
        seed,
    };
    let vertex_fixture = build_vertices_fixture(spec);
    canbench_rs::bench_fn(move || {
        let graph = load_fixture(spec, &vertex_fixture);
        build_edges_from_fixture(&graph, topology, n, seed);
        black_box(graph.forward_dgap().edges.memories().read_num_edges());
    })
}

/// Measures only the stable-memory snapshot phase for a fully built fixture.
fn bench_snapshot_fixture(
    variant: BenchVariant,
    topology: GraphTopology,
    n: usize,
    seed: u64,
) -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let _graph = build_variant_graph(variant, topology, n, seed);
    canbench_rs::bench_fn(move || {
        let bytes = wipe::snapshot_stable_memory();
        black_box(bytes.len());
    })
}

#[derive(Clone, Copy)]
struct SegmentMaintainBenchCase {
    leaf: SegmentEdgeCounts,
    rebalance: RebalanceDecision,
    queue_len: u64,
    thresholds: SegmentMaintainThresholds,
    expected: SegmentMaintainAction,
}

fn segment_maintain_thresholds() -> SegmentMaintainThresholds {
    SegmentMaintainThresholds::default()
}

fn segment_maintain_case(
    leaf: SegmentEdgeCounts,
    rebalance: RebalanceDecision,
    queue_len: u64,
    expected: SegmentMaintainAction,
) -> SegmentMaintainBenchCase {
    SegmentMaintainBenchCase {
        leaf,
        rebalance,
        queue_len,
        thresholds: segment_maintain_thresholds(),
        expected,
    }
}

/// Measures the policy function that decides whether a touched segment should be ignored,
/// enqueued, or maintained inline.
///
/// These micro-benchmarks do not build a graph. They exist to track the decision logic cost and
/// to pin expected decisions for representative SEC/rebalance/queue-pressure combinations.
fn bench_segment_maintain(case: SegmentMaintainBenchCase) -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    canbench_rs::bench_fn(move || {
        let action = segment_maintenance_decision(
            case.leaf,
            case.rebalance,
            case.queue_len,
            &case.thresholds,
        );
        assert_eq!(
            action, case.expected,
            "segment maintenance decision changed"
        );
        black_box(action);
    })
}

fn build_chain_stores(n: usize) -> BenchStores {
    let format = graph_format_for_vertices(n);
    let mgr = MemoryManager::init(DefaultMemoryImpl::default());
    let vertices = SlotMap::new(mgr.get(MemoryId::new(0))).expect("vertex SlotMap");
    let edges = BenchEdgeStore::new(DgapGraphMemories::new(
        mgr.get(MemoryId::new(1)),
        mgr.get(MemoryId::new(2)),
    ));
    edges
        .format_new(
            format.elem_capacity,
            format.segment_count,
            format.segment_size,
            0,
        )
        .expect("format_new edge region");

    let stores = DgapStores::new(vertices, edges);
    for _ in 0..n {
        stores.insert_vertex(empty_vertex()).expect("insert_vertex");
    }
    for (src, dst) in generate_chain_edges(n) {
        stores
            .insert_edge(src, BenchEdge::default().with_neighbor_vid(dst))
            .expect("insert_edge");
    }
    stores
        .refresh_slab_occupied_tail_meta()
        .expect("refresh_slab_occupied_tail_meta");
    stores.sync_pma_meta().expect("sync_pma_meta");
    stores
}

/// Micro-benchmark for the physical slab compaction primitive on a very small chain graph.
///
/// This is a localized storage benchmark, useful for detecting regressions in the underlying
/// in-place removal path without involving delete queues or logical filtering.
#[bench(raw)]
fn bench_remove_slab_physically_chain_32() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let stores = build_chain_stores(32);
    canbench_rs::bench_fn(|| {
        stores
            .edges
            .remove_slab_edge_at_local_index_physically(&stores.vertices, 0, 0)
            .expect("remove_slab");
    })
}

/// Same as `bench_remove_slab_physically_chain_32`, but on a longer chain to show how the
/// primitive behaves once metadata and cursor positions are larger.
#[bench(raw)]
fn bench_remove_slab_physically_chain_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let stores = build_chain_stores(1024);
    canbench_rs::bench_fn(|| {
        stores
            .edges
            .remove_slab_edge_at_local_index_physically(&stores.vertices, 0, 0)
            .expect("remove_slab");
    })
}

/// Physical slab removal benchmark targeting a near-tail vertex in a 1024-node chain.
///
/// This distinguishes "head of structure" removals from removals closer to the occupied slab
/// tail, where fewer downstream elements typically need to move.
#[bench(raw)]
fn bench_remove_slab_physically_tail_vertex_chain_1024() -> canbench_rs::BenchResult {
    wipe::wipe_stable_memory();
    let n = 1024usize;
    let stores = build_chain_stores(n);
    let vid = n - 2;
    canbench_rs::bench_fn(|| {
        stores
            .edges
            .remove_slab_edge_at_local_index_physically(&stores.vertices, vid, 0)
            .expect("remove_slab tail");
    })
}

/// Fixture build benchmark for the row-tombstone variant on an 8k uniform-random sparse graph.
///
/// Use this to track graph construction cost when no deleted-vertex index is present.
#[bench(raw)]
fn bench_build_fixture_row_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_fixture(
        BenchVariant::Row,
        GraphTopology::UniformRandomSparse,
        8_192,
        0x0801,
    )
}

/// Fixture build benchmark for the roaring-backed sparse-deleted variant on an 8k
/// uniform-random sparse graph.
#[bench(raw)]
fn bench_build_fixture_sparse_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_fixture(
        BenchVariant::Sparse,
        GraphTopology::UniformRandomSparse,
        8_192,
        0x0802,
    )
}

/// Fixture build benchmark for the bitset-backed dense-deleted variant on an 8k
/// uniform-random sparse graph.
#[bench(raw)]
fn bench_build_fixture_dense_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_fixture(
        BenchVariant::Dense,
        GraphTopology::UniformRandomSparse,
        8_192,
        0x0803,
    )
}

#[bench(raw)]
fn bench_build_vertices_row_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_vertices(BenchVariant::Row, 8_192)
}

#[bench(raw)]
fn bench_build_vertices_sparse_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_vertices(BenchVariant::Sparse, 8_192)
}

#[bench(raw)]
fn bench_build_vertices_dense_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_vertices(BenchVariant::Dense, 8_192)
}

#[bench(raw)]
fn bench_build_edges_row_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_edges(BenchVariant::Row, GraphTopology::UniformRandomSparse, 8_192, 0x0811)
}

#[bench(raw)]
fn bench_build_edges_sparse_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_edges(BenchVariant::Sparse, GraphTopology::UniformRandomSparse, 8_192, 0x0812)
}

#[bench(raw)]
fn bench_build_edges_dense_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_build_edges(BenchVariant::Dense, GraphTopology::UniformRandomSparse, 8_192, 0x0813)
}

#[bench(raw)]
fn bench_snapshot_fixture_row_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_snapshot_fixture(BenchVariant::Row, GraphTopology::UniformRandomSparse, 8_192, 0x0821)
}

#[bench(raw)]
fn bench_snapshot_fixture_sparse_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_snapshot_fixture(
        BenchVariant::Sparse,
        GraphTopology::UniformRandomSparse,
        8_192,
        0x0822,
    )
}

#[bench(raw)]
fn bench_snapshot_fixture_dense_uniform_random_sparse_8192() -> canbench_rs::BenchResult {
    bench_snapshot_fixture(
        BenchVariant::Dense,
        GraphTopology::UniformRandomSparse,
        8_192,
        0x0823,
    )
}

/// Deletes the hub-first 10% vertex set from a 1k hub-star graph using `RowTombstone`.
///
/// This is the low-level baseline for high-degree vertex deletion without logical-read support.
#[bench(raw)]
fn bench_delete_vertex_row_hub_star_1024_hub_first_d10pct() -> canbench_rs::BenchResult {
    bench_delete_vertices(
        BenchVariant::Row,
        GraphTopology::HubStar,
        1024,
        DeletePattern::HubFirst,
        DeleteDensity::D10Pct,
        0x1001,
    )
}

/// Same hub-star vertex-delete case as the row variant, but using the roaring-backed
/// sparse-deleted graph.
#[bench(raw)]
fn bench_delete_vertex_sparse_hub_star_1024_hub_first_d10pct() -> canbench_rs::BenchResult {
    bench_delete_vertices(
        BenchVariant::Sparse,
        GraphTopology::HubStar,
        1024,
        DeletePattern::HubFirst,
        DeleteDensity::D10Pct,
        0x1002,
    )
}

/// Same hub-star vertex-delete case as the row/sparse variants, but using the bitset-backed
/// dense-deleted graph.
#[bench(raw)]
fn bench_delete_vertex_dense_hub_star_1024_hub_first_d10pct() -> canbench_rs::BenchResult {
    bench_delete_vertices(
        BenchVariant::Dense,
        GraphTopology::HubStar,
        1024,
        DeletePattern::HubFirst,
        DeleteDensity::D10Pct,
        0x1003,
    )
}

/// Deletes a 1% uniform-random vertex set from a 32k uniform-random sparse graph using the
/// roaring-backed sparse-deleted variant.
#[bench(raw)]
fn bench_delete_vertex_sparse_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_delete_vertices(
        BenchVariant::Sparse,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x2001,
    )
}

/// Same 32k uniform-random sparse vertex-delete case as the sparse benchmark, but using the
/// bitset-backed dense-deleted variant.
#[bench(raw)]
fn bench_delete_vertex_dense_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_delete_vertices(
        BenchVariant::Dense,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x2002,
    )
}

/// Runs GC after hub-first vertex deletion on a 1k hub-star graph using `RowTombstone`.
///
/// This isolates queue draining and physical cleanup cost after the logical delete phase.
#[bench(raw)]
fn bench_gc_step_row_hub_star_1024_hub_first_d10pct() -> canbench_rs::BenchResult {
    bench_gc_after_delete(
        BenchVariant::Row,
        GraphTopology::HubStar,
        1024,
        DeletePattern::HubFirst,
        DeleteDensity::D10Pct,
        0x3001,
    )
}

/// Same post-delete GC case as the row variant, but using the roaring-backed sparse-deleted
/// graph.
#[bench(raw)]
fn bench_gc_step_sparse_hub_star_1024_hub_first_d10pct() -> canbench_rs::BenchResult {
    bench_gc_after_delete(
        BenchVariant::Sparse,
        GraphTopology::HubStar,
        1024,
        DeletePattern::HubFirst,
        DeleteDensity::D10Pct,
        0x3002,
    )
}

/// Same post-delete GC case as the row/sparse variants, but using the bitset-backed
/// dense-deleted graph.
#[bench(raw)]
fn bench_gc_step_dense_hub_star_1024_hub_first_d10pct() -> canbench_rs::BenchResult {
    bench_gc_after_delete(
        BenchVariant::Dense,
        GraphTopology::HubStar,
        1024,
        DeletePattern::HubFirst,
        DeleteDensity::D10Pct,
        0x3003,
    )
}

/// Deletes a 1% uniform-random set of directed edges from a 32k uniform-random sparse graph
/// using `RowTombstone`.
///
/// The selected edges are validated against the reopened fixture before timing starts.
#[bench(raw)]
fn bench_delete_edge_row_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_delete_edges(
        BenchVariant::Row,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0xD00D_0001,
    )
}

/// Same 32k uniform-random sparse edge-delete case as the row benchmark, but using the
/// roaring-backed sparse-deleted graph.
#[bench(raw)]
fn bench_delete_edge_sparse_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_delete_edges(
        BenchVariant::Sparse,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0xD00D_0001,
    )
}

/// Same 32k uniform-random sparse edge-delete case as the row/sparse benchmarks, but using the
/// bitset-backed dense-deleted graph.
#[bench(raw)]
fn bench_delete_edge_dense_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_delete_edges(
        BenchVariant::Dense,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0xD00D_0001,
    )
}

/// Deletes a clustered 10% edge set from a 32k clustered-community graph using `RowTombstone`.
///
/// This case stresses local neighborhood churn and segment-local maintenance pressure.
#[bench(raw)]
fn bench_delete_edge_row_clustered_community_32768_clustered_contiguous_d10pct()
-> canbench_rs::BenchResult {
    bench_delete_edges(
        BenchVariant::Row,
        GraphTopology::ClusteredCommunity,
        32_768,
        DeletePattern::ClusteredContiguous,
        DeleteDensity::D10Pct,
        0xD00D_0010,
    )
}

/// Same clustered-community edge-delete case as the row benchmark, but using the roaring-backed
/// sparse-deleted graph.
#[bench(raw)]
fn bench_delete_edge_sparse_clustered_community_32768_clustered_contiguous_d10pct()
-> canbench_rs::BenchResult {
    bench_delete_edges(
        BenchVariant::Sparse,
        GraphTopology::ClusteredCommunity,
        32_768,
        DeletePattern::ClusteredContiguous,
        DeleteDensity::D10Pct,
        0xD00D_0010,
    )
}

/// Same clustered-community edge-delete case as the row/sparse benchmarks, but using the
/// bitset-backed dense-deleted graph.
#[bench(raw)]
fn bench_delete_edge_dense_clustered_community_32768_clustered_contiguous_d10pct()
-> canbench_rs::BenchResult {
    bench_delete_edges(
        BenchVariant::Dense,
        GraphTopology::ClusteredCommunity,
        32_768,
        DeletePattern::ClusteredContiguous,
        DeleteDensity::D10Pct,
        0xD00D_0010,
    )
}

/// Runs GC after clustered-community deletions using `RowTombstone`.
///
/// This shows how quickly tombstones and deleted-vertex incidents are physically cleaned when
/// deletes are spatially clustered in the graph.
#[bench(raw)]
fn bench_gc_step_row_clustered_community_32768_clustered_contiguous_d10pct()
-> canbench_rs::BenchResult {
    bench_gc_after_delete(
        BenchVariant::Row,
        GraphTopology::ClusteredCommunity,
        32_768,
        DeletePattern::ClusteredContiguous,
        DeleteDensity::D10Pct,
        0x3004,
    )
}

/// Same clustered-community post-delete GC case as the row benchmark, but using the
/// roaring-backed sparse-deleted graph.
#[bench(raw)]
fn bench_gc_step_sparse_clustered_community_32768_clustered_contiguous_d10pct()
-> canbench_rs::BenchResult {
    bench_gc_after_delete(
        BenchVariant::Sparse,
        GraphTopology::ClusteredCommunity,
        32_768,
        DeletePattern::ClusteredContiguous,
        DeleteDensity::D10Pct,
        0x3005,
    )
}

/// Same clustered-community post-delete GC case as the row/sparse benchmarks, but using the
/// bitset-backed dense-deleted graph.
#[bench(raw)]
fn bench_gc_step_dense_clustered_community_32768_clustered_contiguous_d10pct()
-> canbench_rs::BenchResult {
    bench_gc_after_delete(
        BenchVariant::Dense,
        GraphTopology::ClusteredCommunity,
        32_768,
        DeletePattern::ClusteredContiguous,
        DeleteDensity::D10Pct,
        0x3006,
    )
}

/// Measures logical reads on the roaring-backed sparse-deleted graph after a 1% uniform-random
/// delete set has been applied to a 32k uniform-random sparse graph.
#[bench(raw)]
fn bench_logical_read_sparse_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_logical_read(
        BenchVariant::Sparse,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x4001,
    )
}

/// Same logical-read case as the sparse benchmark, but using the bitset-backed dense-deleted
/// graph.
#[bench(raw)]
fn bench_logical_read_dense_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_logical_read(
        BenchVariant::Dense,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x4002,
    )
}

/// Measures raw storage scans on `RowTombstone` after a 1% uniform-random delete set has been
/// applied to a 32k uniform-random sparse graph.
#[bench(raw)]
fn bench_raw_read_row_uniform_random_sparse_32768_uniform_random_d1pct() -> canbench_rs::BenchResult
{
    bench_raw_read(
        BenchVariant::Row,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x4101,
    )
}

/// Same raw-read case as the row benchmark, but using the roaring-backed sparse-deleted graph.
#[bench(raw)]
fn bench_raw_read_sparse_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_raw_read(
        BenchVariant::Sparse,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x4102,
    )
}

/// Same raw-read case as the row/sparse benchmarks, but using the bitset-backed dense-deleted
/// graph.
#[bench(raw)]
fn bench_raw_read_dense_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_raw_read(
        BenchVariant::Dense,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x4103,
    )
}

/// Read-heavy scenario benchmark for the roaring-backed sparse-deleted graph on a 32k
/// uniform-random sparse topology.
///
/// The workload alternates mostly reads with occasional deletes and periodic GC, representing the
/// primary service-default comparison path.
#[bench(raw)]
fn bench_scenario_read_heavy_sparse_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_workload(
        BenchVariant::Sparse,
        WorkloadKind::ReadHeavy,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x5001,
    )
}

/// Same read-heavy scenario as the sparse benchmark, but using the bitset-backed dense-deleted
/// graph.
#[bench(raw)]
fn bench_scenario_read_heavy_dense_uniform_random_sparse_32768_uniform_random_d1pct()
-> canbench_rs::BenchResult {
    bench_workload(
        BenchVariant::Dense,
        WorkloadKind::ReadHeavy,
        GraphTopology::UniformRandomSparse,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D1Pct,
        0x5002,
    )
}

/// Mixed workload benchmark for the roaring-backed sparse-deleted graph on a 32k power-law
/// topology with a 10% delete set.
///
/// This case is used for default-selection guidance because it mixes hot reads with sustained
/// churn and periodic cleanup.
#[bench(raw)]
fn bench_scenario_mixed_sparse_power_law_32768_uniform_random_d10pct() -> canbench_rs::BenchResult {
    bench_workload(
        BenchVariant::Sparse,
        WorkloadKind::Mixed,
        GraphTopology::PowerLaw,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D10Pct,
        0x5003,
    )
}

/// Same mixed power-law workload as the sparse benchmark, but using the bitset-backed
/// dense-deleted graph.
#[bench(raw)]
fn bench_scenario_mixed_dense_power_law_32768_uniform_random_d10pct() -> canbench_rs::BenchResult {
    bench_workload(
        BenchVariant::Dense,
        WorkloadKind::Mixed,
        GraphTopology::PowerLaw,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D10Pct,
        0x5004,
    )
}

/// Delete-heavy workload benchmark for the roaring-backed sparse-deleted graph on a 32k
/// power-law topology.
///
/// This is the main stress case for deciding when dense deleted-index tracking becomes attractive.
#[bench(raw)]
fn bench_scenario_delete_heavy_sparse_power_law_32768_uniform_random_d10pct()
-> canbench_rs::BenchResult {
    bench_workload(
        BenchVariant::Sparse,
        WorkloadKind::DeleteHeavy,
        GraphTopology::PowerLaw,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D10Pct,
        0x5005,
    )
}

/// Same delete-heavy power-law workload as the sparse benchmark, but using the bitset-backed
/// dense-deleted graph.
#[bench(raw)]
fn bench_scenario_delete_heavy_dense_power_law_32768_uniform_random_d10pct()
-> canbench_rs::BenchResult {
    bench_workload(
        BenchVariant::Dense,
        WorkloadKind::DeleteHeavy,
        GraphTopology::PowerLaw,
        32_768,
        DeletePattern::UniformRandom,
        DeleteDensity::D10Pct,
        0x5006,
    )
}

/// Segment-maintenance policy micro-benchmark for a small leaf with negligible garbage.
///
/// Expected result is `Noop`, so this case catches regressions that accidentally over-schedule
/// maintenance for healthy segments.
#[bench(raw)]
fn bench_segment_maintain_small_noop() -> canbench_rs::BenchResult {
    bench_segment_maintain(segment_maintain_case(
        SegmentEdgeCounts {
            actual: 10,
            total: 100,
            tombstone: 1,
        },
        RebalanceDecision::Noop,
        0,
        SegmentMaintainAction::Noop,
    ))
}

/// Segment-maintenance policy micro-benchmark for a small leaf with enough garbage to justify
/// background work, but not enough urgency for inline maintenance.
#[bench(raw)]
fn bench_segment_maintain_small_enqueue() -> canbench_rs::BenchResult {
    bench_segment_maintain(segment_maintain_case(
        SegmentEdgeCounts {
            actual: 95,
            total: 100,
            tombstone: 5,
        },
        RebalanceDecision::Noop,
        0,
        SegmentMaintainAction::Enqueue,
    ))
}

/// Segment-maintenance policy micro-benchmark for a large leaf where the tombstone score alone
/// should enqueue maintenance.
#[bench(raw)]
fn bench_segment_maintain_large_enqueue_by_score() -> canbench_rs::BenchResult {
    bench_segment_maintain(segment_maintain_case(
        SegmentEdgeCounts {
            actual: 2900,
            total: 3000,
            tombstone: 100,
        },
        RebalanceDecision::Noop,
        0,
        SegmentMaintainAction::Enqueue,
    ))
}

/// Segment-maintenance policy micro-benchmark where the strict garbage ratio crosses the inline
/// threshold, so maintenance should happen immediately.
#[bench(raw)]
fn bench_segment_maintain_strict_inline() -> canbench_rs::BenchResult {
    bench_segment_maintain(segment_maintain_case(
        SegmentEdgeCounts {
            actual: 7,
            total: 10,
            tombstone: 3,
        },
        RebalanceDecision::Noop,
        0,
        SegmentMaintainAction::InlineNow,
    ))
}

/// Segment-maintenance policy micro-benchmark that verifies queue pressure flips the decision
/// from enqueue to inline maintenance.
#[bench(raw)]
fn bench_segment_maintain_queue_pressure_inline() -> canbench_rs::BenchResult {
    bench_segment_maintain(segment_maintain_case(
        SegmentEdgeCounts {
            actual: 95,
            total: 100,
            tombstone: 5,
        },
        RebalanceDecision::Noop,
        64,
        SegmentMaintainAction::InlineNow,
    ))
}

/// Segment-maintenance policy micro-benchmark where a pending PMA rebalance window should force
/// an enqueue decision even without tombstone pressure.
#[bench(raw)]
fn bench_segment_maintain_rebalance_window_enqueue() -> canbench_rs::BenchResult {
    bench_segment_maintain(segment_maintain_case(
        SegmentEdgeCounts {
            actual: 10,
            total: 100,
            tombstone: 0,
        },
        RebalanceDecision::RebalanceWindow {
            left_vertex: 0,
            right_vertex: 32,
            pma_idx: 16,
        },
        0,
        SegmentMaintainAction::Enqueue,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_set_respects_requested_density() {
        let n = 1_000usize;
        assert_eq!(
            make_delete_set(DeletePattern::HubFirst, n, DeleteDensity::D10Pct, 1).len(),
            100
        );
        assert_eq!(
            make_delete_set(DeletePattern::HubFirst, n, DeleteDensity::D1Pct, 1).len(),
            10
        );
    }

    #[test]
    fn uniform_random_edges_are_deterministic() {
        let a = generate_uniform_random_sparse_edges(64, 42);
        let b = generate_uniform_random_sparse_edges(64, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn chain_topology_has_expected_edge_count() {
        assert_eq!(generate_edges(GraphTopology::Chain, 32, 1).len(), 31);
    }

    #[test]
    fn clustered_community_builds_inter_community_bridges() {
        let edges = generate_edges(GraphTopology::ClusteredCommunity, 128, 1);
        assert!(edges.contains(&(0, 64)));
        assert!(edges.contains(&(64, 0)));
    }

    fn assert_graph_matches_generated_edges(
        variant: BenchVariant,
        topology: GraphTopology,
        n: usize,
        seed: u64,
    ) {
        wipe::wipe_stable_memory();
        let graph = build_variant_graph(variant, topology, n, seed);
        let expected = generate_edges(topology, n, seed);
        let mut actual = collect_actual_edges(&graph, n);
        let mut expected_sorted = expected.clone();
        actual.sort_unstable();
        expected_sorted.sort_unstable();
        assert_eq!(actual, expected_sorted, "out-edge set mismatch");
        for (src, dst) in expected {
            assert!(
                graph.raw_in_has_neighbor(dst, src),
                "missing reverse in-edge view for {src}->{dst}"
            );
        }
    }

    #[test]
    fn direct_builder_matches_chain_topology() {
        assert_graph_matches_generated_edges(BenchVariant::Row, GraphTopology::Chain, 64, 1);
    }

    #[test]
    fn direct_builder_matches_uniform_random_sparse_topology() {
        assert_graph_matches_generated_edges(
            BenchVariant::Dense,
            GraphTopology::UniformRandomSparse,
            256,
            42,
        );
    }

    #[test]
    fn direct_builder_matches_clustered_community_topology() {
        assert_graph_matches_generated_edges(
            BenchVariant::Sparse,
            GraphTopology::ClusteredCommunity,
            256,
            7,
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn edge_delete_candidates_are_valid_for_reopened_fixture() {
        wipe::wipe_stable_memory();
        let spec = FixtureSpec {
            variant: BenchVariant::Dense,
            topology: GraphTopology::UniformRandomSparse,
            n: 1_024,
            seed: 7,
        };
        let fixture = build_fixture(spec);
        let graph = load_fixture(spec, &fixture);
        let delete_edges = make_edge_delete_set_for_graph(
            &graph,
            spec.topology,
            spec.n,
            DeletePattern::UniformRandom,
            DeleteDensity::D1Pct,
            spec.seed,
        );
        assert_eq!(
            delete_edges.len(),
            delete_count(spec.n, DeleteDensity::D1Pct)
        );
        for (src, dst) in delete_edges {
            assert!(
                graph.has_raw_edge_pair(src, dst),
                "missing raw pair {src}->{dst}"
            );
        }
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn variant_graph_smoke_build_delete_and_gc() {
        for variant in [BenchVariant::Row, BenchVariant::Sparse, BenchVariant::Dense] {
            wipe::wipe_stable_memory();
            let graph = build_variant_graph(variant, GraphTopology::UniformRandomSparse, 1_024, 7);
            let delete_set =
                make_delete_set(DeletePattern::UniformRandom, 1_024, DeleteDensity::D1Pct, 7);
            apply_delete_set(&graph, &delete_set);
            let _ = run_gc_to_completion(&graph, 32);
        }
    }
}
