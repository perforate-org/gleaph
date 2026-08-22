//! ForceAtlas2 layout (§15.1).
//!
//! The primary dynamic graph layout. The public API wraps the implementation
//! behind [`ForceAtlas2`] rather than exposing raw settings, keeping the layout
//! implementation replaceable.
//!
//! This is a self-contained implementation of the ForceAtlas2 force model
//! (repulsion, attraction, gravity) with FA2 local speed adaptation plus two
//! settling extensions the reference lacks because Gephi runs a fixed
//! iteration count while this engine must report `Settled`:
//!
//! - a displacement dead-band (nodes below [`DISPLACEMENT_EPSILON`] rest for
//!   the iteration), and
//! - a global cooling schedule ([`COOLING_FACTOR`] decay per iteration,
//!   reset on rebuild), which guarantees termination even around stiff
//!   equilibria where local adaptation alone cannot damp coherent jitter.
//!
//! Pairwise forces saturate at [`MIN_DISTANCE`]: magnitudes use the floored
//! distance while directions stay true, so overlapped nodes push apart at
//! bounded strength instead of exploding (`1/d²`) or going weightless.
//!
//! Repulsion runs on one of two paths, selected by node count: an exact
//! radius-cutoff spatial grid (nodes beyond the radius do not interact; the
//! default), or a Barnes-Hut quadtree approximating all-pairs long-range
//! repulsion in O(N·log N) — opt-in via [`ForceAtlas2::with_barnes_hut_threshold`]
//! because it only wins on densely clustered topologies (§37). Both paths
//! apply FA2 degree mass (degree + 1).
//!
//! Execution model: every force phase and the movement pass are data-parallel
//! per node (rayon) at or above [`PAR_MIN_NODES`]; below it, serial drivers
//! run the same physics with the cheaper scheduling the scale allows — the
//! grid pair walk shares one distance evaluation between both endpoints
//! instead of the parallel path's per-node single-sided scans. Wasm-family
//! builds (browser, WASI) link no rayon at all — no OS worker threads exist
//! there — and always take the serial drivers. Parallel workers never share accumulators: grid repulsion
//! scans each node's own 3×3 neighborhood single-sided, Barnes-Hut resolves
//! one root descent per node with thread-local stacks, and attraction gathers
//! each node's incident pulls through a CSR adjacency refreshed whenever the
//! projection's topology revision changes. A qualifying pair's arithmetic may
//! be evaluated twice under rayon (once per endpoint); that buys race-free
//! parallel writes without per-thread force buffers or atomics.
//!
//! Numerical note: the force model computes through [`f32::algebraic_*`]
//! arithmetic (see the component-wise helpers below), which permits
//! reassociation, FMA contraction, and reciprocal-multiply. Results are not
//! bit-reproducible across runs (parallel workers also reduce forces in
//! nondeterministic order), compiler versions, or platforms; that is
//! acceptable here because this is an animated simulation whose tests assert
//! convergence, never exact coordinates. The algebraic helpers must stay out
//! of hit-testing, paint geometry, and viewport math, where epsilon
//! comparisons rely on stable operation-by-operation precision.

use glam::Vec2;
#[cfg(not(target_family = "wasm"))]
use rayon::prelude::*;

use super::graph::{LayoutGraph, LayoutState};
use super::{LayoutBudget, LayoutEngine, LayoutProgress};

/// Maximum per-iteration displacement (world units) a node may move. The FA2
/// reference has no such cap and relies purely on its speed formula; ours
/// bounds the damage while mass-weighted repulsion is still violent early on
/// (a degree-256 hub carries mass 257 and slams crowded neighbors hard).
/// Sized so early spread/collapse phases progress in visible strides without
/// letting saturated nodes teleport through each other every iteration.
const MAX_STEP: f32 = 4.0;
/// Per-iteration decay of the global cooling factor. FA2's local speed
/// adaptation cannot damp coherent high-force oscillation (its convergence
/// factor saturates whenever forces are large), so layouts around stiff,
/// densely packed equilibria would jitter above the rest threshold forever.
/// A global cooling schedule — the standard settling companion elsewhere
/// (d3's alpha decay, OpenOrd annealing) — multiplies every movement and
/// guarantees termination within ~`COOLING_FACTOR` decades of iterations.
/// [`LayoutEngine::rebuild`] resets it, so topology changes get fresh energy.
const COOLING_FACTOR: f32 = 0.995;
/// Distance floor for pairwise forces: below one world unit, pairs interact
/// as if exactly one unit apart. Repulsion magnitude `s·mi·mj/d²` otherwise
/// diverges at touching distance, slamming dense clusters into a capped-
/// speed chaos of clamp-floor impulses that never settles. The floor
/// saturates the MAGNITUDE only (`s·mi·mj / MIN_DISTANCE²` applied along
/// the true separation direction): flooring the distance inside the
/// delta-scaled impulse would make overlapped pairs weightless instead.
const MIN_DISTANCE: f32 = 1.0;
/// Desired displacement below which a node rests for the iteration. FA2 has
/// no intrinsic decay, so residual force noise around an equilibrium keeps
/// every node micro-jittering forever; Gephi sidesteps this by running a
/// fixed iteration count, we must report `Settled`. Sized at a tenth of a
/// world unit — imperceptible at any reasonable viewport scale — and safely
/// above the measured equilibrium jitter band (~0.011–0.017), which would
/// otherwise straddle a smaller threshold and flicker on/off forever.
const DISPLACEMENT_EPSILON: f32 = 0.1;
/// Node count at or above which force phases run on rayon workers instead of
/// identical serial drivers over the same per-node kernels. Measured crossover
/// (layout_bench): at ≤2500 nodes the per-pass thread wake/join cost dominates
/// the iteration work itself — hub/256 went 167 µs → 1.60 ms and grid/20x20
/// 210 µs → 1.57 ms when unconditionally parallel — while ≥4096 nodes win on
/// favorable topologies (hub/4096 38.8 ms → 11.9 ms, grid_bh/100x100 32.8 ms
/// → 11.3 ms). Like `bh_threshold`, this is a node-count proxy whose outcome
/// topology modulates (sparse uniform grids gain least); a finer activation
/// policy stays deferred (§37). Scheduling differs only — physics does not.
/// Irrelevant on wasm targets, where rayon is not linked and every size is
/// serial.
const PAR_MIN_NODES: usize = 4096;

/// Whether rayon workers exist on this target: every wasm-family target
/// lacks OS worker threads and the dependency is not even linked there.
/// (`target_family = "wasm"` also covers a future wasm64, which an arch
/// check would miss.)
const RAYON_AVAILABLE: bool = cfg!(not(target_family = "wasm"));

/// Component-wise [`f32::algebraic_add`].
#[inline]
fn algebraic_add(a: Vec2, b: Vec2) -> Vec2 {
    Vec2::new(a.x.algebraic_add(b.x), a.y.algebraic_add(b.y))
}

/// Component-wise [`f32::algebraic_sub`].
#[inline]
fn algebraic_sub(a: Vec2, b: Vec2) -> Vec2 {
    Vec2::new(a.x.algebraic_sub(b.x), a.y.algebraic_sub(b.y))
}

/// Scalar [`f32::algebraic_mul`] applied to both components.
#[inline]
fn algebraic_mul_scalar(v: Vec2, s: f32) -> Vec2 {
    Vec2::new(v.x.algebraic_mul(s), v.y.algebraic_mul(s))
}

/// Packs a cell coordinate into one sortable key. Negative components wrap to
/// a fixed order via `as u32`; only injectivity matters, not numeric order.
fn cell_key(cx: i32, cy: i32) -> u64 {
    ((cx as u32 as u64) << 32) | cy as u32 as u64
}

/// Neighbor offsets that visit each adjacent cell pair exactly once: together
/// with their negations they cover all eight Moore directions.
const FORWARD_STENCIL: [(i32, i32); 4] = [(1, 0), (1, 1), (0, 1), (1, -1)];

/// A uniform spatial hash over node positions, rebuilt every iteration into
/// reused flat buffers. Nodes are grouped by grid cell into one contiguous
/// `entries` array (`offsets` is its per-cell index), so pair enumeration
/// walks memory sequentially instead of chasing per-cell heap vectors and
/// hashing every candidate lookup.
///
/// With `cell_size == repulsion_radius`, only cells that are adjacent or equal
/// can hold a pair within the radius. Each such pair is visited exactly once:
/// intra-cell pairs directly, cross-cell pairs through [`FORWARD_STENCIL`].
#[derive(Debug, Clone, Default)]
struct RepulsionGrid {
    /// Grid pitch; equals the repulsion radius.
    cell_size: f32,
    /// Unique packed cell coordinates, sorted ascending.
    cells: Vec<u64>,
    /// Per-cell start offset into `entries`, plus one end sentinel.
    offsets: Vec<u32>,
    /// Node indices grouped by cell.
    entries: Vec<u32>,
    /// Scratch reused across rebuilds; empty between calls.
    scratch: Vec<(u64, u32)>,
}

impl RepulsionGrid {
    fn new(cell_size: f32) -> Self {
        Self {
            cell_size,
            cells: Vec::new(),
            offsets: Vec::new(),
            entries: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Regroup `positions` by grid cell, reusing all buffers. The pitch tracks
    /// `radius` so the neighborhood scan stays 3×3 regardless of settings.
    fn rebuild(&mut self, positions: &[Vec2], radius: f32) {
        self.cell_size = radius;
        let cs = self.cell_size;
        self.scratch.clear();
        self.cells.clear();
        self.offsets.clear();
        self.entries.clear();
        for (i, p) in positions.iter().enumerate() {
            let key = cell_key((p.x / cs).floor() as i32, (p.y / cs).floor() as i32);
            self.scratch.push((key, i as u32));
        }
        self.scratch.sort_unstable();
        for si in 0..self.scratch.len() {
            let (key, i) = self.scratch[si];
            if self.cells.last() != Some(&key) {
                self.cells.push(key);
                self.offsets.push(self.entries.len() as u32);
            }
            self.entries.push(i);
        }
        self.offsets.push(self.entries.len() as u32);
    }

    /// Entry range of the cell at `(cx, cy)`; empty if unoccupied.
    fn cell_entries(&self, cx: i32, cy: i32) -> core::ops::Range<usize> {
        let key = cell_key(cx, cy);
        let lo = self.cells.partition_point(|&k| k < key);
        let hi = self.cells.partition_point(|&k| k <= key);
        self.offsets[lo] as usize..self.offsets[hi] as usize
    }

    /// Visit every unordered pair of nodes whose cells are adjacent or equal,
    /// applying `visit` once per pair. The serial repulsion driver: pairs share
    /// one distance evaluation between both endpoints and cells are walked
    /// sequentially, which measures ~3x faster than per-node neighborhood
    /// scans below [`PAR_MIN_NODES`]. The parallel driver uses
    /// [`Self::accumulate_node`] instead because shared accumulators cannot be
    /// written from many threads.
    fn for_each_pair(&self, visit: &mut impl FnMut(usize, usize)) {
        for ci in 0..self.cells.len() {
            let cx = (self.cells[ci] >> 32) as u32 as i32;
            let cy = self.cells[ci] as u32 as i32;
            let range = self.offsets[ci] as usize..self.offsets[ci + 1] as usize;

            for ai in range.clone() {
                for bi in ai + 1..range.end {
                    visit(self.entries[ai] as usize, self.entries[bi] as usize);
                }
            }

            for (dx, dy) in FORWARD_STENCIL {
                let neighbor = self.cell_entries(cx + dx, cy + dy);
                for ai in range.clone() {
                    for bi in neighbor.clone() {
                        visit(self.entries[ai] as usize, self.entries[bi] as usize);
                    }
                }
            }
        }
    }

    /// Accumulate the repulsion received by node `i` alone, scanning only the
    /// cells adjacent to — and including — `i`'s own cell. Single-sided mirror
    /// of [`Self::for_each_pair`]: exactly the same neighbor pairs contribute
    /// with the same saturated-magnitude impulse, but each worker touches only
    /// its own accumulator, so the parallel pass needs no races at the cost of
    /// evaluating each qualifying pair twice (once per endpoint). Native-only:
    /// wasm embeds always drive repulsion through [`Self::for_each_pair`].
    #[cfg(not(target_family = "wasm"))]
    fn accumulate_node(
        &self,
        i: usize,
        positions: &[Vec2],
        masses: &[f32],
        scaling: f32,
        radius: f32,
    ) -> Vec2 {
        let p = positions[i];
        let cx = (p.x / self.cell_size).floor() as i32;
        let cy = (p.y / self.cell_size).floor() as i32;
        let mut acc = Vec2::ZERO;
        for dy in -1..=1i32 {
            for dx in -1..=1i32 {
                let range = self.cell_entries(cx + dx, cy + dy);
                for entry in &self.entries[range] {
                    let j = *entry as usize;
                    if j == i {
                        continue;
                    }
                    // Force on `i` points away from `j`, matching the pair
                    // walk's `i`-side accumulation.
                    let delta = algebraic_sub(positions[i], positions[j]);
                    let dist = delta.length();
                    if dist == 0.0 || dist > radius {
                        continue;
                    }
                    let eff = dist.max(MIN_DISTANCE);
                    let strength = scaling
                        .algebraic_mul(masses[i])
                        .algebraic_mul(masses[j])
                        .algebraic_div(eff.algebraic_mul(eff));
                    acc = algebraic_add(acc, algebraic_mul_scalar(delta, strength / dist));
                }
            }
        }
        acc
    }
}

/// Sentinel for absent quadrant children and empty point chains.
const NO_INDEX: u32 = u32::MAX;

/// One cell of the Barnes-Hut quadtree.
#[derive(Debug, Clone)]
struct BhCell {
    /// Cell center and half side length.
    x: f32,
    y: f32,
    half: f32,
    /// Quadrant child cells (`NO_INDEX` = no child there yet). A cell is a
    /// leaf exactly while all four are absent.
    children: [u32; 4],
    /// Head of this leaf's node chain (`NO_INDEX` = empty leaf).
    head: u32,
    /// Aggregated subtree center of mass; valid after [`BhTree::aggregate`].
    com: Vec2,
    /// Aggregated subtree mass; valid after aggregate.
    mass: f32,
}

impl BhCell {
    fn new(x: f32, y: f32, half: f32) -> Self {
        Self {
            x,
            y,
            half,
            children: [NO_INDEX; 4],
            head: NO_INDEX,
            com: Vec2::ZERO,
            mass: 0.0,
        }
    }
}

/// Flat quadtree for Barnes-Hut repulsion over node positions. Rebuilt every
/// iteration while reusing its buffers. Nodes that coincide (or pile up past
/// the depth floor) stay as an exact chain inside one deepest cell, so those
/// interactions are enumerated precisely instead of being merged away.
///
/// Children always carry higher arena indices than their parent because cells
/// are only ever appended during insertion; aggregation therefore works as a
/// single reverse index pass without an explicit post-order traversal.
#[derive(Debug, Clone, Default)]
struct BhTree {
    cells: Vec<BhCell>,
    /// Chain links indexed by node id (`next_same[node]` = next node sharing
    /// that node's bucket cell).
    next_same: Vec<u32>,
}

/// Quadrant of `p` relative to a cell center: bit 0 selects the +x half,
/// bit 1 the +y half.
fn quadrant(p: Vec2, cx: f32, cy: f32) -> usize {
    usize::from(p.x >= cx) | (usize::from(p.y >= cy) << 1)
}

impl BhTree {
    /// Rebuild the tree over `positions`, then aggregate subtree mass and
    /// center of mass bottom-up.
    fn build(&mut self, positions: &[Vec2], masses: &[f32]) {
        self.cells.clear();
        self.next_same.clear();
        self.next_same.resize(positions.len(), NO_INDEX);
        let Some(&first) = positions.first() else {
            return;
        };
        let mut min = first;
        let mut max = first;
        for p in &positions[1..] {
            min = min.min(*p);
            max = max.max(*p);
        }
        let center = algebraic_mul_scalar(algebraic_add(min, max), 0.5);
        let extent = algebraic_sub(max, min);
        let mut half = extent.x.max(extent.y) * 0.5;
        if half <= 0.0 || !half.is_finite() {
            half = 0.5;
        }
        // A sliver of growth keeps boundary points strictly inside their
        // quadrant tests even under float rounding at the outer edges.
        half *= 1.0001;
        self.cells.push(BhCell::new(center.x, center.y, half));

        for pi in 0..positions.len() {
            self.insert(pi, positions);
        }
        self.aggregate(positions, masses);
    }

    fn insert(&mut self, pi: usize, positions: &[Vec2]) {
        // Depth floor: below this cell size, distinct positions cannot be
        // separated reliably in f32 anyway, so pile them into one exact chain.
        const MIN_HALF: f32 = 1e-5;
        let p = positions[pi];
        let mut c = 0usize;
        let (mut cx, mut cy, mut half) = {
            let root = &self.cells[0];
            (root.x, root.y, root.half)
        };
        loop {
            if self.cells[c].head != NO_INDEX {
                if half <= MIN_HALF {
                    let head = self.cells[c].head;
                    self.next_same[pi] = head;
                    self.cells[c].head = pi as u32;
                    return;
                }
                // Occupied leaf: move its chain one level down so the held
                // nodes and the incoming point can separate by quadrant.
                let held = self.cells[c].head;
                self.cells[c].head = NO_INDEX;
                let hq = quadrant(positions[held as usize], cx, cy);
                let hc = self.child_leaf(c, hq, cx, cy, half);
                self.cells[hc].head = held;
            } else if self.cells[c].children == [NO_INDEX; 4] {
                // Empty leaf: place the incoming point here.
                self.cells[c].head = pi as u32;
                return;
            }
            let q = quadrant(p, cx, cy);
            c = self.child_leaf(c, q, cx, cy, half);
            half *= 0.5;
            cx += if q & 1 == 1 { half } else { -half };
            cy += if q & 2 == 2 { half } else { -half };
        }
    }

    /// Existing quadrant child or a freshly created empty leaf there.
    fn child_leaf(&mut self, parent: usize, q: usize, px: f32, py: f32, phalf: f32) -> usize {
        let existing = self.cells[parent].children[q];
        if existing != NO_INDEX {
            return existing as usize;
        }
        let chalf = phalf * 0.5;
        let x = px + if q & 1 == 1 { chalf } else { -chalf };
        let y = py + if q & 2 == 2 { chalf } else { -chalf };
        let id = self.cells.len() as u32;
        self.cells.push(BhCell::new(x, y, chalf));
        self.cells[parent].children[q] = id;
        id as usize
    }

    /// Bottom-up mass and center-of-mass aggregation. Reverse index order is
    /// sufficient because children always out-index their parent.
    fn aggregate(&mut self, positions: &[Vec2], masses: &[f32]) {
        for i in (0..self.cells.len()).rev() {
            let mut m = 0.0f32;
            let mut s = Vec2::ZERO;
            let mut p = self.cells[i].head;
            while p != NO_INDEX {
                let pm = masses[p as usize];
                m += pm;
                s = algebraic_add(s, algebraic_mul_scalar(positions[p as usize], pm));
                p = self.next_same[p as usize];
            }
            for &child in &self.cells[i].children {
                if child != NO_INDEX {
                    let cc = &self.cells[child as usize];
                    m += cc.mass;
                    s = algebraic_add(s, algebraic_mul_scalar(cc.com, cc.mass));
                }
            }
            let cell = &mut self.cells[i];
            cell.mass = m;
            cell.com = if m > 0.0 {
                algebraic_mul_scalar(s, 1.0f32.algebraic_div(m))
            } else {
                Vec2::ZERO
            };
        }
    }

    /// Approximate repulsion received by node `i`: walk the quadtree from the
    /// root with the θ opening criterion, merging a cell into its center of
    /// mass when `side / distance < theta`, otherwise opening its quadrants.
    /// Cells containing the query point always fail the criterion for theta ≤ 1
    /// and are resolved down to exact chains, so self-interaction never occurs.
    /// `stack` is caller-provided scratch so parallel workers never share
    /// traversal state.
    fn node_repulsion(
        &self,
        i: usize,
        positions: &[Vec2],
        masses: &[f32],
        theta: f32,
        scaling: f32,
        stack: &mut Vec<usize>,
    ) -> Vec2 {
        let mut acc = Vec2::ZERO;
        if self.cells.is_empty() {
            return acc;
        }
        stack.clear();
        stack.push(0);
        while let Some(c) = stack.pop() {
            let cell = &self.cells[c];
            if cell.mass <= 0.0 {
                continue;
            }
            // Leaves resolve exactly through their point chains.
            if cell.head != NO_INDEX {
                let mut p = cell.head;
                while p != NO_INDEX {
                    acc = algebraic_add(
                        acc,
                        self.pair_repulsion(i, p as usize, positions, masses, scaling),
                    );
                    p = self.next_same[p as usize];
                }
                continue;
            }
            let delta = algebraic_sub(cell.com, positions[i]);
            // No floor here: the opening criterion must see the true
            // distance so a cell containing the query node always fails
            // the theta test (its com lies within the cell).
            let dist = delta.length();
            let side = cell.half * 2.0;
            if side < theta * dist {
                // Far enough: merge the whole subtree into its center of
                // mass. The floor saturates the magnitude only; direction
                // and criterion keep the true distance, so overlapping
                // clusters still push apart instead of going weightless.
                let eff = dist.max(MIN_DISTANCE);
                let strength =
                    scaling.algebraic_mul(masses[i]).algebraic_mul(cell.mass) / (eff * eff);
                acc = algebraic_add(acc, algebraic_mul_scalar(delta, strength / dist));
            } else {
                for &child in &cell.children {
                    if child != NO_INDEX {
                        stack.push(child as usize);
                    }
                }
            }
        }
        acc
    }

    /// Serial wrapper over [`Self::node_repulsion`] for the brute-force test
    /// oracle; the engine drives it through rayon instead.
    #[cfg(test)]
    fn accumulate(
        &self,
        positions: &[Vec2],
        masses: &[f32],
        theta: f32,
        scaling: f32,
        forces: &mut [Vec2],
    ) {
        if self.cells.is_empty() {
            return;
        }
        let mut stack: Vec<usize> = Vec::new();
        for (i, force) in forces.iter_mut().enumerate() {
            *force = algebraic_add(
                *force,
                self.node_repulsion(i, positions, masses, theta, scaling, &mut stack),
            );
        }
    }

    /// Exact repulsion impulse received by node `i` from node `j`.
    fn pair_repulsion(
        &self,
        i: usize,
        j: usize,
        positions: &[Vec2],
        masses: &[f32],
        scaling: f32,
    ) -> Vec2 {
        if j == i {
            return Vec2::ZERO;
        }
        let delta = algebraic_sub(positions[j], positions[i]);
        let dist = delta.length();
        if dist == 0.0 {
            // Exact coincidence has no separation direction; the reference
            // implementation skips such pairs too.
            return Vec2::ZERO;
        }
        // The floor saturates the `1/d²` magnitude only; direction stays the
        // true separation, so overlapped pairs keep pushing apart instead of
        // going weightless below one world unit.
        let eff = dist.max(MIN_DISTANCE);
        let strength = scaling.algebraic_mul(masses[i]).algebraic_mul(masses[j]) / (eff * eff);
        algebraic_mul_scalar(delta, strength / dist)
    }
}

/// ForceAtlas2 layout engine.
#[derive(Debug, Clone)]
pub struct ForceAtlas2 {
    /// Repulsion scaling factor.
    scaling: f32,
    /// Gravity strength pulling nodes toward the origin.
    gravity: f32,
    /// Use `log(1 + dist)` attraction instead of linear attraction.
    lin_log: bool,
    /// Divisor applied to every node's per-iteration movement (FA2
    /// `slowDown`). Values above 1 slow global progress; below 1 risk jitter.
    slow_down: f32,
    /// Distance beyond which two nodes no longer repel each other. Repulsion
    /// falls off as `1/dist^2`, so beyond this radius it is negligible; the
    /// spatial grid only tests nodes within this radius, making repulsion
    /// O(N·k) instead of O(N²).
    repulsion_radius: f32,
    /// Total displacement below which the layout is considered settled.
    settled_threshold: f32,
    /// Per-node FA2 convergence factor in `(0, 1]`, algorithm-specific state
    /// (§11.4): carries local speed adaptation across iterations.
    convergence: Vec<f32>,
    /// Previous iteration's force vector per node, used for the swinging /
    /// traction balance (§11.4).
    prev_forces: Vec<Vec2>,
    /// Reused per-iteration force buffer, so `iterate` does not allocate.
    forces: Vec<Vec2>,
    /// Spatial hash for the repulsion pass on small graphs, rebuilt each
    /// iteration while reusing its buffers across iterations. Pairs beyond
    /// `repulsion_radius` do not interact on this path.
    grid: RepulsionGrid,
    /// Barnes-Hut quadtree for the repulsion pass on large graphs, rebuilt
    /// each iteration. Approximates all-pairs (long-range) repulsion in
    /// O(N·log N) via center-of-mass cells (§37).
    tree: BhTree,
    /// Node count at or above which the Barnes-Hut path replaces the exact
    /// radius-cutoff grid. Disabled by default (`usize::MAX`): the two paths
    /// compute different amounts of work — the cutoff grid only touches each
    /// node's local neighborhood, while the quadtree approximates all-pairs
    /// long-range repulsion — so the grid wins wherever local density is low
    /// (measured: up to ~6x faster on sparse uniform layouts), and the
    /// quadtree wins where nodes cluster densely (measured: ~45% faster on a
    /// 4096-leaf ring). Activation stays opt-in until a topology-aware
    /// policy exists (§37).
    bh_threshold: usize,
    /// Barnes-Hut opening ratio: a cell merges into its center of mass when
    /// `side / distance < theta`. Values up to ~1.4 guarantee that a cell
    /// containing the query node is always subdivided (exact self-exclusion);
    /// larger values trade that guarantee for speed.
    theta: f32,
    /// Node mass = degree + 1 (FA2 weighting): scales repulsion and gravity.
    masses: Vec<f32>,
    /// Global cooling multiplier in `(0, 1]` applied to every movement;
    /// decays each iteration and resets on rebuild (§37).
    cooling: f32,
    /// CSR adjacency over the projection's edges for gather-based attraction:
    /// `attr_targets[attr_offsets[i]..attr_offsets[i+1]]` holds one entry per
    /// incident edge endpoint of node `i`, so duplicate edges pull twice just
    /// as they did in the edge-list walk this replaces.
    attr_offsets: Vec<u32>,
    attr_targets: Vec<u32>,
    /// Topology revision the current attraction adjacency was built from;
    /// `u64::MAX` until the first build so an initial revision of 0 still
    /// triggers it.
    attr_revision: u64,
}

impl Default for ForceAtlas2 {
    fn default() -> Self {
        Self {
            scaling: 1.0,
            gravity: 0.1,
            lin_log: false,
            slow_down: 1.0,
            repulsion_radius: 100.0,
            settled_threshold: 0.001,
            convergence: Vec::new(),
            prev_forces: Vec::new(),
            forces: Vec::new(),
            grid: RepulsionGrid::new(100.0),
            tree: BhTree::default(),
            bh_threshold: usize::MAX,
            theta: 1.0,
            masses: Vec::new(),
            cooling: 1.0,
            attr_offsets: Vec::new(),
            attr_targets: Vec::new(),
            attr_revision: u64::MAX,
        }
    }
}

impl ForceAtlas2 {
    /// Set the repulsion scaling factor.
    pub fn with_scaling(mut self, scaling: f32) -> Self {
        self.scaling = scaling;
        self
    }

    /// Set the gravity strength.
    pub fn with_gravity(mut self, gravity: f32) -> Self {
        self.gravity = gravity;
        self
    }

    /// Enable `log(1 + dist)` attraction.
    pub fn with_lin_log(mut self, lin_log: bool) -> Self {
        self.lin_log = lin_log;
        self
    }

    /// Set the FA2 `slowDown` divisor applied to every node's per-iteration
    /// movement.
    pub fn with_slow_down(mut self, slow_down: f32) -> Self {
        self.slow_down = slow_down;
        self
    }

    /// Set the repulsion radius: the distance beyond which two nodes no longer
    /// repel each other. A smaller radius makes repulsion cheaper (fewer nearby
    /// nodes per grid cell) at the cost of allowing nodes to pack more tightly.
    pub fn with_repulsion_radius(mut self, radius: f32) -> Self {
        self.repulsion_radius = radius;
        self
    }

    /// Set the total-displacement threshold below which the layout settles.
    pub fn with_settled_threshold(mut self, threshold: f32) -> Self {
        self.settled_threshold = threshold;
        self
    }

    /// Enable the Barnes-Hut repulsion path for graphs with at least
    /// `threshold` nodes, replacing the exact radius-cutoff grid. The
    /// quadtree approximates all-pairs long-range repulsion in O(N·log N),
    /// so it pays off on densely clustered topologies (hub-and-ring, tight
    /// communities) and loses to the cutoff grid on sparse uniform layouts.
    pub fn with_barnes_hut_threshold(mut self, threshold: usize) -> Self {
        self.bh_threshold = threshold;
        self
    }

    /// Set the Barnes-Hut opening ratio `theta` (accuracy/speed trade-off).
    /// Lower is more accurate; values above ~1.4 may let a cell containing
    /// the query node merge into its own center of mass.
    pub fn with_theta(mut self, theta: f32) -> Self {
        self.theta = theta;
        self
    }
}

impl LayoutEngine for ForceAtlas2 {
    fn rebuild(&mut self, graph: &LayoutGraph, state: &mut LayoutState) {
        // Rebuild algorithm-specific state. Positions are owned by the scene
        // and preserved across rebuilds (§11.6). Tunable parameters (scaling,
        // gravity, slowDown, ...) are preserved across rebuilds.
        let n = graph.node_count();
        self.forces.resize(n, Vec2::ZERO);
        self.prev_forces.resize(n, Vec2::ZERO);
        // Fresh nodes start fully adaptive; established nodes keep their
        // adapted factor across incremental topology changes.
        self.convergence.resize(n, 1.0);
        // Topology changes get fresh layout energy.
        self.cooling = 1.0;
        // FA2 mass = degree + 1: every incident edge counts, directed or not.
        self.masses.clear();
        self.masses.resize(n, 1.0);
        for edge in &graph.edges {
            self.masses[edge.source.0 as usize] += 1.0;
            self.masses[edge.target.0 as usize] += 1.0;
        }
        self.tree.next_same.resize(n, NO_INDEX);
        self.refresh_attraction_adjacency(graph);
        let _ = state;
    }

    fn step(
        &mut self,
        graph: &LayoutGraph,
        state: &mut LayoutState,
        budget: LayoutBudget,
    ) -> LayoutProgress {
        let n = graph.node_count();
        if n == 0 {
            return LayoutProgress::Settled;
        }
        if self.convergence.len() != n {
            self.convergence.resize(n, 1.0);
            self.prev_forces.resize(n, Vec2::ZERO);
            self.forces.resize(n, Vec2::ZERO);
        }
        if self.masses.len() != n {
            // Topology changed without a rebuild: fall back to unit mass
            // rather than indexing stale masses.
            self.masses.clear();
            self.masses.resize(n, 1.0);
        }
        // Same staleness guard for the attraction adjacency: incremental
        // topology updates may arrive without an engine rebuild, so the CSR
        // is refreshed by revision check rather than trusted from rebuild.
        self.refresh_attraction_adjacency(graph);

        let mut last_displacement = 0.0f32;
        for _ in 0..budget.max_iterations {
            last_displacement = self.iterate(graph, state);
        }

        // `iterate` returns the total displacement across all nodes; compare the
        // average per-node displacement so the threshold is independent of graph
        // size. A small graph with a few jittering nodes must still settle.
        let avg_displacement = last_displacement / n as f32;
        if avg_displacement < self.settled_threshold {
            LayoutProgress::Settled
        } else {
            let stability = (1.0 - (avg_displacement / 10.0).min(1.0)).max(0.0);
            LayoutProgress::Running {
                stability: Some(stability),
            }
        }
    }
}

impl ForceAtlas2 {
    /// Rebuild the attraction adjacency when the projection changed. Every
    /// `step` pays only an O(1) revision-and-length check, so incremental
    /// topology updates that arrive without [`LayoutEngine::rebuild`] stay
    /// correct; the O(E) rebuild itself runs only on actual changes.
    fn refresh_attraction_adjacency(&mut self, graph: &LayoutGraph) {
        let n = graph.node_count();
        let endpoints = 2 * graph.edge_count();
        if self.attr_revision == graph.topology_revision
            && self.attr_offsets.len() == n + 1
            && self.attr_targets.len() == endpoints
        {
            return;
        }
        let mut offsets = vec![0u32; n + 1];
        for edge in &graph.edges {
            offsets[edge.source.0 as usize + 1] += 1;
            offsets[edge.target.0 as usize + 1] += 1;
        }
        for window in 1..=n {
            offsets[window] += offsets[window - 1];
        }
        let mut targets = vec![0u32; endpoints];
        let mut cursor = offsets[..n].to_vec();
        for edge in &graph.edges {
            targets[cursor[edge.source.0 as usize] as usize] = edge.target.0;
            cursor[edge.source.0 as usize] += 1;
            targets[cursor[edge.target.0 as usize] as usize] = edge.source.0;
            cursor[edge.target.0 as usize] += 1;
        }
        self.attr_offsets = offsets;
        self.attr_targets = targets;
        self.attr_revision = graph.topology_revision;
    }

    /// Run a single force-model iteration, returning total displacement.
    ///
    /// The projection is read only through engine-owned adjacency and buffers,
    /// so the graph argument is unused here; `step` refreshes the attraction
    /// adjacency against its revision.
    fn iterate(&mut self, _graph: &LayoutGraph, state: &mut LayoutState) -> f32 {
        let forces = &mut self.forces;
        forces.fill(Vec2::ZERO);

        // Repulsion: below the Barnes-Hut threshold, nodes repel within
        // `repulsion_radius` through the exact CSR grid (O(N·k)); at or above
        // it, the quadtree approximates all-pairs long-range repulsion in
        // O(N·log N). Both apply FA2 degree mass. Both run one task per node
        // over disjoint accumulators when [`PAR_MIN_NODES`] is met.
        let n = state.positions.len();
        let parallel = n >= PAR_MIN_NODES && RAYON_AVAILABLE;
        if n >= self.bh_threshold {
            let mut tree = std::mem::take(&mut self.tree);
            tree.build(&state.positions, &self.masses);
            let (positions, masses, theta, scaling) =
                (&state.positions, &self.masses, self.theta, self.scaling);
            if parallel {
                #[cfg(not(target_family = "wasm"))]
                forces
                    .par_iter_mut()
                    .enumerate()
                    .for_each_init(Vec::new, |stack, (i, force)| {
                        *force = tree.node_repulsion(i, positions, masses, theta, scaling, stack);
                    });
            } else {
                let mut stack: Vec<usize> = Vec::new();
                for (i, force) in forces.iter_mut().enumerate() {
                    *force = tree.node_repulsion(i, positions, masses, theta, scaling, &mut stack);
                }
            }
            self.tree = tree;
        } else {
            let mut grid = std::mem::take(&mut self.grid);
            grid.rebuild(&state.positions, self.repulsion_radius);
            let (positions, masses, scaling, radius) = (
                &state.positions,
                &self.masses,
                self.scaling,
                self.repulsion_radius,
            );
            if parallel {
                #[cfg(not(target_family = "wasm"))]
                forces.par_iter_mut().enumerate().for_each(|(i, force)| {
                    *force = grid.accumulate_node(i, positions, masses, scaling, radius);
                });
            } else {
                // Serial pair walk: one distance evaluation shared by both
                // endpoints and sequential cell traversal — measurably faster
                // than per-node scans at this scale.
                grid.for_each_pair(&mut |i, j| {
                    let delta = algebraic_sub(positions[i], positions[j]);
                    let dist = delta.length();
                    if dist == 0.0 || dist > radius {
                        return;
                    }
                    let eff = dist.max(MIN_DISTANCE);
                    let strength = scaling
                        .algebraic_mul(masses[i])
                        .algebraic_mul(masses[j])
                        .algebraic_div(eff.algebraic_mul(eff));
                    let impulse = algebraic_mul_scalar(delta, strength / dist);
                    forces[i] = algebraic_add(forces[i], impulse);
                    forces[j] = algebraic_sub(forces[j], impulse);
                });
            }
            self.grid = grid;
        }

        // Attraction: every edge pulls its endpoints together, gathered per
        // node over the CSR adjacency so workers write disjoint accumulators.
        // One entry per incident endpoint keeps duplicate-edge pull exact; a
        // self-loop contributes a zero-length pull, exactly as the edge walk
        // it replaces.
        let lin_log = self.lin_log;
        let (offsets, targets, positions) =
            (&self.attr_offsets, &self.attr_targets, &state.positions);
        let gather_attraction = |force: &mut Vec2, i: usize| {
            *force = algebraic_add(
                *force,
                attraction_pull(lin_log, offsets, targets, positions, i),
            );
        };
        if parallel {
            #[cfg(not(target_family = "wasm"))]
            forces
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, force)| gather_attraction(force, i));
        } else {
            for (i, force) in forces.iter_mut().enumerate() {
                gather_attraction(force, i);
            }
        }

        // Gravity: pull every node toward the origin, weighted by its mass,
        // with CONSTANT magnitude along the unit direction like the FA2
        // reference. A spring-law pull growing with distance saturates every
        // node onto the step cap and stretches the initial collapse into
        // thousands of extra iterations.
        for (force, (pos, mass)) in forces
            .iter_mut()
            .zip(state.positions.iter().zip(&self.masses))
        {
            let dist = pos.length();
            if dist > 0.0 {
                let factor = self.gravity.algebraic_mul(*mass).algebraic_div(dist);
                *force = algebraic_sub(*force, algebraic_mul_scalar(*pos, factor));
            }
        }

        // Apply forces with FA2 local speed adaptation. There is no global
        // velocity: each node moves along its current force vector by an
        // amount adapted from how much its force changed between iterations
        // (`swinging`, mass-weighted) versus its steady pull (`traction`).
        // The per-node `convergence` factor carries the adaptation across
        // iterations, so oscillating regions slow themselves down and the
        // layout converges instead of jittering around an equilibrium.
        //
        // Both drivers iterate zipped disjoint slices so each node's adaptive
        // state has exactly one owner; skipped nodes contribute zero to the
        // displacement total. Pinned nodes return before touching their
        // adaptive state, keeping it frozen until unpinned.
        let cooling = self.cooling;
        let slow_down = self.slow_down;
        let (forces_ref, masses_ref) = (&self.forces, &self.masses);
        let pinned = &state.pinned;
        let convergence = &mut self.convergence;
        let prev_forces = &mut self.prev_forces;
        #[cfg(not(target_family = "wasm"))]
        let total: f32 = if parallel {
            state
                .positions
                .par_iter_mut()
                .zip(convergence.par_iter_mut())
                .zip(prev_forces.par_iter_mut())
                .enumerate()
                .map(|(i, ((position, convergence_i), prev_force))| {
                    apply_node_motion(
                        forces_ref[i],
                        masses_ref[i],
                        pinned[i],
                        cooling,
                        slow_down,
                        position,
                        convergence_i,
                        prev_force,
                    )
                })
                .sum()
        } else {
            let mut total = 0.0f32;
            for (i, ((position, convergence_i), prev_force)) in state
                .positions
                .iter_mut()
                .zip(convergence.iter_mut())
                .zip(prev_forces.iter_mut())
                .enumerate()
            {
                total += apply_node_motion(
                    forces_ref[i],
                    masses_ref[i],
                    pinned[i],
                    cooling,
                    slow_down,
                    position,
                    convergence_i,
                    prev_force,
                );
            }
            total
        };
        #[cfg(target_family = "wasm")]
        let total: f32 = {
            let mut total = 0.0f32;
            for (i, ((position, convergence_i), prev_force)) in state
                .positions
                .iter_mut()
                .zip(convergence.iter_mut())
                .zip(prev_forces.iter_mut())
                .enumerate()
            {
                total += apply_node_motion(
                    forces_ref[i],
                    masses_ref[i],
                    pinned[i],
                    cooling,
                    slow_down,
                    position,
                    convergence_i,
                    prev_force,
                );
            }
            total
        };
        // Global cooling decays once per iteration, after applying movements.
        self.cooling = (self.cooling * COOLING_FACTOR).max(0.0);
        total
    }
}

/// Attraction pull received by node `i`, gathered over the CSR adjacency:
/// one pull per incident edge endpoint, so duplicate edges pull twice and a
/// self-loop contributes a zero-length pull.
fn attraction_pull(
    lin_log: bool,
    offsets: &[u32],
    targets: &[u32],
    positions: &[Vec2],
    i: usize,
) -> Vec2 {
    let mut acc = Vec2::ZERO;
    for entry in &targets[offsets[i] as usize..offsets[i + 1] as usize] {
        let delta = algebraic_sub(positions[*entry as usize], positions[i]);
        let dist = delta.length().max(MIN_DISTANCE);
        let fa = if lin_log {
            (1.0f32.algebraic_add(dist)).ln()
        } else {
            dist
        };
        // `dir * fa == delta * (fa / dist)`: one division instead of two.
        acc = algebraic_add(acc, algebraic_mul_scalar(delta, fa.algebraic_div(dist)));
    }
    acc
}

/// FA2 local speed adaptation applied to one node: updates its carried
/// convergence factor and previous force, moves its position along the force
/// vector, and returns its clamped displacement contribution. Shared kernel
/// behind the serial and parallel drivers; mutable node state arrives as
/// disjoint references so neither driver duplicates the physics.
#[allow(clippy::too_many_arguments)]
fn apply_node_motion(
    force: Vec2,
    mass: f32,
    pinned: bool,
    cooling: f32,
    slow_down: f32,
    position: &mut Vec2,
    convergence_i: &mut f32,
    prev_force: &mut Vec2,
) -> f32 {
    if pinned {
        return 0.0;
    }
    let prev = *prev_force;
    let swing_root = algebraic_sub(prev, force)
        .length()
        .algebraic_mul(mass)
        .sqrt();
    let swing_denom = swing_root.algebraic_add(1.0);
    let traction = algebraic_add(prev, force).length() * 0.5;
    let nodespeed = (*convergence_i)
        .algebraic_mul((1.0 + traction).ln())
        .algebraic_div(swing_denom);
    // Update convergence from this iteration's balance before moving;
    // clamped to 1 so adaptation never amplifies beyond raw forces.
    *convergence_i = nodespeed
        .algebraic_mul(force.length_squared())
        .algebraic_div(swing_denom)
        .sqrt()
        .min(1.0);
    *prev_force = force;
    // Dead-band: below the epsilon the node is effectively at rest. Freeze it
    // so equilibrium noise cannot jitter it forever and the layout can report
    // settled. State above stays fresh, so a real force (drag, topology
    // change) moves it again immediately.
    let desired = force
        .length()
        .algebraic_mul(nodespeed * cooling / slow_down);
    if desired < DISPLACEMENT_EPSILON {
        return 0.0;
    }
    // [`MAX_STEP`] applies only when one iteration would otherwise fling the
    // node across the graph.
    let shrink = (MAX_STEP / desired).min(1.0);
    *position = algebraic_add(
        *position,
        algebraic_mul_scalar(force, nodespeed * shrink * cooling / slow_down),
    );
    desired.algebraic_mul(shrink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeDirection, Graph};
    use crate::layout::graph::{LayoutIndex, LayoutNode};

    fn project(graph: &Graph<(), ()>) -> (LayoutGraph, LayoutState) {
        let node_ids: Vec<_> = graph.nodes().map(|(id, _)| id).collect();
        let mut state = LayoutState::new();
        state.resize(node_ids.len());
        let edges = graph
            .edges()
            .map(|(_, e)| {
                let source = node_ids.iter().position(|&x| x == e.source).unwrap() as u32;
                let target = node_ids.iter().position(|&x| x == e.target).unwrap() as u32;
                crate::layout::graph::LayoutEdge {
                    source: LayoutIndex(source),
                    target: LayoutIndex(target),
                    direction: e.direction,
                }
            })
            .collect();
        let lg = LayoutGraph {
            nodes: vec![LayoutNode {}; node_ids.len()],
            edges,
            node_ids,
            topology_revision: 0,
        };
        (lg, state)
    }

    #[test]
    fn force_atlas2_moves_nodes_toward_edges() {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Undirected, ());

        let (lg, mut state) = project(&g);
        // Start far apart.
        state.positions[0] = Vec2::new(-100.0, 0.0);
        state.positions[1] = Vec2::new(100.0, 0.0);

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        let mut progress = LayoutProgress::Running { stability: None };
        for _ in 0..200 {
            progress = fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
        }

        // Nodes should have moved closer together and settled: a two-node
        // graph equilibrates quickly under adaptive speed plus cooling.
        let dist = (state.positions[0] - state.positions[1]).length();
        assert!(dist < 200.0, "distance should shrink, got {dist}");
        assert_eq!(progress, LayoutProgress::Settled);
    }

    #[test]
    fn pinned_nodes_do_not_move() {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Undirected, ());

        let (lg, mut state) = project(&g);
        state.positions[0] = Vec2::new(-100.0, 0.0);
        state.positions[1] = Vec2::new(100.0, 0.0);
        state.pinned.set(0, true);

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        for _ in 0..50 {
            fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
        }

        assert_eq!(state.positions[0], Vec2::new(-100.0, 0.0));
    }

    #[test]
    fn empty_graph_settles() {
        let lg = LayoutGraph {
            nodes: vec![],
            edges: vec![],
            node_ids: vec![],
            topology_revision: 0,
        };
        let mut state = LayoutState::new();
        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        assert_eq!(
            fa.step(&lg, &mut state, LayoutBudget::default()),
            LayoutProgress::Settled
        );
    }

    #[test]
    fn repulsion_grid_pairs_match_brute_force() {
        // Scattered points across many cells, including coincident positions.
        let mut pts: Vec<Vec2> = (0..48)
            .map(|i| {
                let t = i as f32;
                Vec2::new(t * 61.7 % 800.0 - 400.0, t * 113.3 % 600.0 - 300.0)
            })
            .collect();
        pts.push(pts[3]);
        pts.push(pts[17]);
        pts.push(Vec2::ZERO);

        let cs = 100.0;
        let cell = |p: Vec2| ((p.x / cs).floor() as i32, (p.y / cs).floor() as i32);
        let mut expected = std::collections::BTreeSet::new();
        for i in 0..pts.len() {
            for j in i + 1..pts.len() {
                let (a, b) = (cell(pts[i]), cell(pts[j]));
                if (a.0 - b.0).abs() <= 1 && (a.1 - b.1).abs() <= 1 {
                    expected.insert((i, j));
                }
            }
        }

        let mut grid = RepulsionGrid::new(cs);
        grid.rebuild(&pts, cs);
        let mut got = std::collections::BTreeSet::new();
        grid.for_each_pair(&mut |i, j| {
            assert!(i != j);
            got.insert((i.min(j), i.max(j)));
        });

        assert_eq!(
            got, expected,
            "grid must visit exactly the adjacent-cell pairs"
        );
    }

    #[test]
    fn repulsion_node_scan_matches_pair_accumulation() {
        // The single-sided per-node scan must reproduce exactly what the
        // double-sided pair walk accumulates: same qualifying pairs, same
        // saturated-magnitude impulses, summed in a different order.
        let mut pts: Vec<Vec2> = (0..48)
            .map(|i| {
                let t = i as f32;
                Vec2::new(t * 61.7 % 800.0 - 400.0, t * 113.3 % 600.0 - 300.0)
            })
            .collect();
        pts.push(pts[3]);
        pts.push(Vec2::ZERO);
        let masses: Vec<f32> = (0..pts.len()).map(|i| (i % 3 + 1) as f32).collect();
        let scaling = 12.0;
        let radius = 150.0;

        let mut grid = RepulsionGrid::new(radius);
        grid.rebuild(&pts, radius);

        let mut want = vec![Vec2::ZERO; pts.len()];
        grid.for_each_pair(&mut |i, j| {
            let delta = pts[i] - pts[j];
            let dist = delta.length();
            if dist == 0.0 || dist > radius {
                return;
            }
            let eff = dist.max(MIN_DISTANCE);
            let strength = scaling * masses[i] * masses[j] / (eff * eff);
            let impulse = delta * (strength / dist);
            want[i] += impulse;
            want[j] -= impulse;
        });

        for (i, got) in (0..pts.len())
            .map(|i| grid.accumulate_node(i, &pts, &masses, scaling, radius))
            .enumerate()
        {
            let err = (got - want[i]).length();
            let scale = want[i].length().max(1.0);
            assert!(
                err < 1e-4 * scale,
                "node {i}: scan {got:?} vs pair {want:?} (err {err})"
            );
        }
    }

    #[test]
    fn attraction_adjacency_lists_both_endpoints_per_edge() {
        // Parallel edges pull twice, a self-loop contributes two zero-length
        // endpoint entries, and every undirected edge appears under both ends.
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        let c = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Undirected, ());
        g.add_edge(a, b, EdgeDirection::Undirected, ());
        g.add_edge(b, c, EdgeDirection::Undirected, ());
        g.add_edge(c, c, EdgeDirection::Undirected, ());

        let (lg, mut state) = project(&g);
        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);

        // Node order is a, b, c. Endpoint multisets:
        // a: [b, b], b: [a, a, c], c: [b, c, c].
        assert_eq!(fa.attr_offsets, vec![0, 2, 5, 8]);
        assert_eq!(fa.attr_targets, vec![1, 1, 0, 0, 2, 1, 2, 2]);
        assert_eq!(fa.attr_revision, lg.topology_revision);
    }

    #[test]
    fn stale_topology_refreshes_attraction_adjacency_without_rebuild() {
        // Incremental topology updates may bypass the engine's rebuild; the
        // step-time revision check must still pick up the changed edges.
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Undirected, ());
        let (mut lg, mut state) = project(&g);

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        assert_eq!(fa.attr_targets.len(), 2);

        // Simulate an incremental delta: one more edge, no rebuild call.
        lg.edges.push(crate::layout::graph::LayoutEdge {
            source: crate::layout::LayoutIndex(1),
            target: crate::layout::LayoutIndex(0),
            direction: EdgeDirection::Undirected,
        });
        lg.topology_revision += 1;

        fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
        assert_eq!(fa.attr_revision, 1);
        assert_eq!(fa.attr_targets.len(), 4);
        assert!(
            fa.attr_targets.starts_with(&[1]),
            "original adjacency preserved at the front"
        );
    }

    #[test]
    fn barnes_hut_matches_brute_force_within_tolerance() {
        // Scattered points with a dense cluster and exact duplicates, so the
        // tree must subdivide deeply and use bucket chains. A tight theta
        // keeps the approximation error well under the asserted bound.
        let mut pts: Vec<Vec2> = (0..64)
            .map(|i| {
                let t = i as f32;
                Vec2::new(t * 61.7 % 800.0 - 400.0, t * 113.3 % 600.0 - 300.0)
            })
            .collect();
        pts.push(pts[3]);
        pts.push(Vec2::ZERO);
        pts.push(Vec2::new(1.0, 1.0));
        pts.push(Vec2::new(1.5, 0.5));
        let masses: Vec<f32> = (0..pts.len()).map(|i| (i % 4 + 1) as f32).collect();

        let scaling = 30.0;

        let mut want = vec![Vec2::ZERO; pts.len()];
        for i in 0..pts.len() {
            for j in 0..pts.len() {
                if i == j {
                    continue;
                }
                let d = pts[j] - pts[i];
                let dist = d.length();
                if dist == 0.0 {
                    continue;
                }
                let eff = dist.max(MIN_DISTANCE);
                want[i] += d * (scaling * masses[i] * masses[j] / (eff * eff) / dist);
            }
        }

        let mut got = vec![Vec2::ZERO; pts.len()];
        let mut tree = BhTree::default();
        tree.build(&pts, &masses);
        tree.accumulate(&pts, &masses, 0.5, scaling, &mut got);

        for k in 0..pts.len() {
            let err = (got[k] - want[k]).length();
            let scale = want[k].length().max(1.0);
            assert!(
                err < 0.05 * scale,
                "node {k}: bh {:?} vs brute {:?} (err {err})",
                got[k],
                want[k]
            );
        }
    }

    /// Step with default settings until [`LayoutProgress::Settled`], capped
    /// at 4000 iterations. Returns the iteration count.
    fn run_until_settled((lg, mut state): (LayoutGraph, LayoutState)) -> u32 {
        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        let budget = LayoutBudget { max_iterations: 1 };
        for iters in 1..=4000 {
            if fa.step(&lg, &mut state, budget) == LayoutProgress::Settled {
                return iters;
            }
        }
        4000
    }

    #[test]
    fn settles_within_iteration_budget() {
        // Termination contract: default settings must settle both shape
        // families well below the probe budget. Counts are printed so
        // convergence behavior stays observable when tuning the force model.
        let hub_iters = run_until_settled(build_ring_hub(256));
        println!("hub/256 settled in {hub_iters} iterations");
        assert!(
            hub_iters < 1500,
            "hub should settle well below the budget, took {hub_iters}"
        );

        let grid_iters = run_until_settled(build_grid(20, 40.0));
        println!("grid/20x20 settled in {grid_iters} iterations");
        assert!(
            grid_iters < 1500,
            "grid should settle well below the budget, took {grid_iters}"
        );
    }

    /// Build a hub graph whose leaves sit on a ring of radius 100.
    fn build_ring_hub(leaves_count: usize) -> (LayoutGraph, LayoutState) {
        let mut g = Graph::new();
        let center = g.add_node(());
        let leaves: Vec<_> = (0..leaves_count).map(|_| g.add_node(())).collect();
        for &leaf in &leaves {
            g.add_edge(center, leaf, EdgeDirection::Undirected, ());
        }
        let (lg, mut state) = project(&g);
        for (i, leaf) in leaves.iter().enumerate() {
            let idx = lg.node_ids.iter().position(|&id| id == *leaf).unwrap();
            let a = (i as f32 / leaves_count as f32) * std::f32::consts::TAU;
            state.positions[idx] = Vec2::new(a.cos() * 100.0, a.sin() * 100.0);
        }
        let center_idx = lg.node_ids.iter().position(|&id| id == center).unwrap();
        state.positions[center_idx] = Vec2::ZERO;
        (lg, state)
    }

    /// Build a `side x side` lattice graph with unit edges spaced `spacing`
    /// world units apart.
    fn build_grid(side: usize, spacing: f32) -> (LayoutGraph, LayoutState) {
        let mut g = Graph::new();
        let mut ids: Vec<Vec<_>> = (0..side).map(|_| Vec::new()).collect();
        for row in &mut ids {
            for _ in 0..side {
                row.push(g.add_node(()));
            }
        }
        for y in 0..side {
            for x in 0..side {
                if x + 1 < side {
                    g.add_edge(ids[y][x], ids[y][x + 1], EdgeDirection::Undirected, ());
                }
                if y + 1 < side {
                    g.add_edge(ids[y][x], ids[y + 1][x], EdgeDirection::Undirected, ());
                }
            }
        }
        let (lg, mut state) = project(&g);
        for y in 0..side {
            for x in 0..side {
                state.positions[y * side + x] = Vec2::new(x as f32 * spacing, y as f32 * spacing);
            }
        }
        (lg, state)
    }

    #[test]
    fn layout_converges_instead_of_oscillating() {
        // A small graph with a hub and several neighbors. With FA2 local speed
        // adaptation, the layout must eventually settle rather than oscillate
        // forever around an equilibrium (which would leave nodes jittering).
        let mut g = Graph::new();
        let ids: Vec<_> = (0..6).map(|_| g.add_node(())).collect();
        for i in 1..ids.len() {
            g.add_edge(ids[0], ids[i], EdgeDirection::Undirected, ());
        }
        let (lg, mut state) = project(&g);
        // Start nodes spread out so the forces are strong.
        for (i, pos) in state.positions.iter_mut().enumerate() {
            *pos = Vec2::new((i as f32 - 2.5) * 40.0, 0.0);
        }

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        let mut progress = LayoutProgress::Running { stability: None };
        for _ in 0..2000 {
            progress = fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
            if progress == LayoutProgress::Settled {
                break;
            }
        }
        assert_eq!(
            progress,
            LayoutProgress::Settled,
            "layout should converge with FA2 adaptive speed"
        );
    }
}
