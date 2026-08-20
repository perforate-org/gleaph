//! Graph runtime (§20).
//!
//! Rendering caches and spatial acceleration live outside the logical graph.
//! The runtime is derived state: it must be reconstructible from authoritative
//! graph and scene state. v0.1 keeps the runtime minimal and tracks revisions
//! so expensive derived structures can be invalidated selectively (§31).
//!
//! The runtime owns the zoom-invariant preprocessing of the graph: the
//! candidate-edge list, parallel groups (`has_reverse`, `parallel`), edge
//! midpoints/normals, the local-density grid, and a uniform-grid spatial index
//! over node positions and edge bounding boxes. These depend only on the
//! topology and geometry revisions, so they are rebuilt once per change and
//! reused across many pan/zoom frames. The paint layer then does only the
//! per-visible-edge work each frame.

use std::hash::BuildHasher;
use std::sync::Arc;

use crate::graph::{EdgeId, Graph, NodeId};
use crate::hash::{HashMap, HashSet};
use crate::paint::{edge_curve_bbox, finite_chord_length, shared_cluster_center};
use crate::scene::GraphScene;
use crate::viewport::WorldBounds;

/// World-space cell size for the uniform-grid spatial index. A node's position
/// is bucketed into one cell; an edge's bounding box is inserted into every
/// cell it covers. The cell size is a fixed world-space constant so the index
/// is zoom-invariant.
const INDEX_CELL_SIZE: f32 = 64.0;
/// Maximum number of cells a query or one edge bounding box may enumerate.
/// Larger regions use the linear candidate path. Keeping this bound explicit
/// prevents a malformed or low-zoom rectangle from turning one frame into an
/// unbounded allocation and iteration.
const MAX_INDEX_CELLS: u128 = 4096;
/// Extra world-space slack added to a visible-region query for edge candidates.
/// An edge's curve can bow outside its source-target bounding box (density bow,
/// parallel fan, cluster bow), so the index stores each edge's full curve
/// bounding box (source, target, and control point). The query rect is still
/// expanded by this amount to guarantee the candidate set is a superset of the
/// edges the precise curve-bbox test would keep. The precise test then filters
/// to the exact visible set.
pub(crate) const EDGE_INDEX_SLACK: f32 = 200.0;

type CellCoordinate = i128;
type Cell = (CellCoordinate, CellCoordinate);
type DensityCell = (i32, i32);

/// A checked rectangle in cell coordinates. Cell coordinates use widened
/// arithmetic because an `f32` world coordinate can map far outside `i32`.
#[derive(Debug, Clone, Copy)]
struct CellRect {
    min: Cell,
    max: Cell,
}

impl CellRect {
    fn from_world(min: glam::Vec2, max: glam::Vec2) -> Option<Self> {
        Self::from_scalars(
            f64::from(min.x),
            f64::from(min.y),
            f64::from(max.x),
            f64::from(max.y),
        )
    }

    fn from_query(bounds: &WorldBounds, margin: f32) -> Option<Self> {
        let margin = f64::from(margin);
        if !margin.is_finite() {
            return None;
        }
        let margin = margin.max(0.0);
        Self::from_scalars(
            f64::from(bounds.min.x) - margin,
            f64::from(bounds.min.y) - margin,
            f64::from(bounds.max.x) + margin,
            f64::from(bounds.max.y) + margin,
        )
    }

    fn from_scalars(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Option<Self> {
        if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
            return None;
        }
        let min = (cell_coordinate(min_x), cell_coordinate(min_y));
        let max = (cell_coordinate(max_x), cell_coordinate(max_y));
        (min.0 <= max.0 && min.1 <= max.1).then_some(Self { min, max })
    }

    /// Return the number of cells only when both widened subtractions and the
    /// multiplication are representable. Callers must check this before using
    /// [`Self::cells`].
    fn cell_count(self) -> Option<u128> {
        let width = self.max.0.checked_sub(self.min.0)?.checked_add(1)?;
        let height = self.max.1.checked_sub(self.min.1)?.checked_add(1)?;
        let width = u128::try_from(width).ok()?;
        let height = u128::try_from(height).ok()?;
        width.checked_mul(height)
    }

    fn within_limit(self) -> bool {
        self.cell_count()
            .is_some_and(|count| count <= MAX_INDEX_CELLS)
    }

    /// Enumerate a bounded rectangle. The caller must first check
    /// [`Self::within_limit`].
    fn cells(self) -> impl Iterator<Item = Cell> {
        let (min_x, min_y) = self.min;
        let (max_x, max_y) = self.max;
        (min_x..=max_x).flat_map(move |x| (min_y..=max_y).map(move |y| (x, y)))
    }
}

fn cell_coordinate(value: f64) -> CellCoordinate {
    (value / f64::from(INDEX_CELL_SIZE)).floor() as CellCoordinate
}

fn density_cell_coordinate(value: f32, cell_size: f32) -> i32 {
    // DensityGrid is a coarse prefilter: signed_densities_for applies the
    // exact distance check after collecting candidates. Rust's float-to-int
    // cast saturates, so coordinates outside the i32 cell-key range can share
    // an edge bucket without dropping a possible neighbor. Candidate-cell
    // arithmetic clamps each query axis before iteration, so a saturated
    // endpoint bucket remains conservative without duplicate visits.
    (value / cell_size).floor() as i32
}

/// The grid cell containing a world-space point.
fn cell_of(p: glam::Vec2) -> Cell {
    (
        cell_coordinate(f64::from(p.x)),
        cell_coordinate(f64::from(p.y)),
    )
}

/// A uniform grid over edge midpoints used to count nearby edges in O(E).
#[derive(Debug, Clone, Default)]
pub struct DensityGrid<S = std::collections::hash_map::RandomState>
where
    S: BuildHasher + Default + Clone,
{
    cell_size: f32,
    cells: HashMap<DensityCell, Vec<usize>, S>,
}

/// A uniform grid over edge midpoints using the default SipHash hasher.
impl DensityGrid {
    /// Bucket the given midpoints into cells of size `radius`.
    pub fn new(midpoints: &[glam::Vec2], radius: f32) -> Self {
        Self::new_with_hasher(
            midpoints,
            radius,
            std::collections::hash_map::RandomState::default(),
        )
    }
}

impl<S> DensityGrid<S>
where
    S: BuildHasher + Default + Clone,
{
    /// Bucket the given midpoints into cells of size `radius` with a custom
    /// hasher.
    pub fn new_with_hasher(midpoints: &[glam::Vec2], radius: f32, hasher: S) -> Self {
        let cell_size = radius;
        let mut cells: HashMap<DensityCell, Vec<usize>, S> =
            HashMap::with_capacity_and_hasher(midpoints.len(), hasher);
        for (i, m) in midpoints.iter().enumerate() {
            let cell = (
                density_cell_coordinate(m.x, cell_size),
                density_cell_coordinate(m.y, cell_size),
            );
            cells.entry(cell).or_default().push(i);
        }
        Self { cell_size, cells }
    }

    /// Indices of edges whose midpoint may lie within `radius` of `midpoint`.
    pub fn candidates(
        &self,
        midpoint: glam::Vec2,
        radius: f32,
    ) -> impl Iterator<Item = usize> + '_ {
        let cell = (
            density_cell_coordinate(midpoint.x, self.cell_size),
            density_cell_coordinate(midpoint.y, self.cell_size),
        );
        let span = (radius / self.cell_size).ceil() as i32;
        // Clamp each axis once before iterating. Saturating every offset can
        // revisit the same boundary cell when the base coordinate is near an
        // i32 limit, which would double-count a neighbor in signed density.
        let min_x = cell.0.saturating_sub(span);
        let max_x = cell.0.saturating_add(span);
        let min_y = cell.1.saturating_sub(span);
        let max_y = cell.1.saturating_add(span);
        (min_x..=max_x).flat_map(move |x| {
            (min_y..=max_y)
                .flat_map(move |y| self.cells.get(&(x, y)).into_iter().flatten().copied())
        })
    }
}

/// Opaque borrowed snapshot of one scene's authoritative runtime inputs.
///
/// A source is created only by [`GraphScene::sync_runtime`]. Keeping the scene
/// itself private here ensures graph data, positions, cluster geometry, and
/// revision markers all come from one immutable scene snapshot.
pub(crate) struct RuntimeSource<'a, NK, EK, N, E, S = std::collections::hash_map::RandomState>
where
    S: BuildHasher + Default + Clone,
{
    scene: &'a GraphScene<NK, EK, N, E, S>,
    source_identity: Arc<()>,
    topology_revision: u64,
    geometry_revision: u64,
}

/// Proof that a runtime and scene describe the same immutable snapshot.
///
/// Values can only be created by [`GraphScene::sync_runtime`]. The shared
/// borrow of the scene prevents topology or geometry mutation for as long as
/// indexed rendering can use the proof, while the runtime borrow prevents its
/// derived state from being replaced independently.
pub struct SyncedGraphRuntime<'a, NK, EK, N, E, S = std::collections::hash_map::RandomState>
where
    S: BuildHasher + Default + Clone,
{
    pub(crate) scene: &'a GraphScene<NK, EK, N, E, S>,
    pub(crate) runtime: &'a GraphRuntime<S>,
}

impl<'a, NK, EK, N, E, S> SyncedGraphRuntime<'a, NK, EK, N, E, S>
where
    S: BuildHasher + Default + Clone,
{
    /// Node ids whose positions may fall within `bounds` expanded by `margin`.
    ///
    /// The proof ties this derived query to the immutable scene snapshot that
    /// supplied the graph, positions, and revisions. Callers that do not hold
    /// this proof must use the linear paint builder instead.
    pub fn visible_nodes(&self, bounds: &WorldBounds, margin: f32) -> Vec<NodeId> {
        self.runtime.visible_nodes(bounds, margin)
    }

    /// Edge candidate indices whose conservative curve bound may intersect
    /// `bounds` expanded by `margin`.
    pub fn visible_edge_candidates(&self, bounds: &WorldBounds, margin: f32) -> Vec<usize> {
        self.runtime.visible_edge_candidates(bounds, margin)
    }

    /// Borrow the zoom-invariant edge preparation for this synchronized
    /// snapshot. The returned arrays are valid only while this proof lives.
    pub fn edges(&self) -> &EdgePrep<S> {
        self.runtime.edges()
    }
}

impl<'a, NK, EK, N, E, S> RuntimeSource<'a, NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    pub(crate) fn from_scene(scene: &'a GraphScene<NK, EK, N, E, S>) -> Self {
        Self {
            scene,
            source_identity: scene.graph().source_identity(),
            topology_revision: scene.topology_revision(),
            geometry_revision: scene.geometry_revision(),
        }
    }
}

/// Per-edge derived geometry, populated during a scene-owned runtime rebuild.
///
/// Holds everything about an edge that depends only on the topology and
/// geometry revisions (not on zoom or the viewport), so the paint layer can
/// build a frame from just the visible edges each frame.
#[derive(Debug, Clone, Default)]
pub struct EdgePrep<S = std::collections::hash_map::RandomState>
where
    S: BuildHasher + Default + Clone,
{
    /// Edge identity per candidate index.
    pub edge_ids: Vec<EdgeId>,
    /// Source world position per candidate index.
    pub source: Vec<glam::Vec2>,
    /// Target world position per candidate index.
    pub target: Vec<glam::Vec2>,
    /// Whether each edge has a reverse edge (target -> source).
    pub has_reverse: Vec<bool>,
    /// For each edge, its position within its parallel group and the group's
    /// size, when the group has more than one edge; `None` for a lone edge.
    pub parallel: Vec<Option<(usize, usize)>>,
    /// World-space midpoint per candidate index.
    pub midpoints: Vec<glam::Vec2>,
    /// Unit left normal per candidate index.
    pub normals: Vec<glam::Vec2>,
    /// Grid over every edge's midpoint for local-density queries.
    pub density_grid: DensityGrid<S>,
}

/// Build the zoom-invariant per-edge preprocessing for a graph.
///
/// `node_position` resolves a node's world-space position. Edges whose endpoints
/// lack a position are omitted, matching how the paint layer skips them.
pub(crate) fn build_edge_prep<N, E, S>(
    graph: &Graph<N, E>,
    node_position: &dyn Fn(NodeId) -> Option<glam::Vec2>,
    hasher: S,
) -> EdgePrep<S>
where
    S: BuildHasher + Default + Clone,
{
    let mut prep = EdgePrep {
        edge_ids: Vec::new(),
        source: Vec::new(),
        target: Vec::new(),
        has_reverse: Vec::new(),
        parallel: Vec::new(),
        midpoints: Vec::new(),
        normals: Vec::new(),
        density_grid: DensityGrid::default(),
    };
    let mut groups: HashMap<(NodeId, NodeId), Vec<usize>, S> = HashMap::with_hasher(hasher.clone());
    for (id, edge) in graph.edges() {
        let (Some(source), Some(target)) = (node_position(edge.source), node_position(edge.target))
        else {
            continue;
        };
        let index = prep.edge_ids.len();
        prep.edge_ids.push(id);
        prep.source.push(source);
        prep.target.push(target);
        let dir = target - source;
        let normal = finite_chord_length(source, target)
            .map(|len| glam::Vec2::new(-dir.y, dir.x) / len)
            .unwrap_or(glam::Vec2::new(0.0, -1.0));
        prep.midpoints.push((source + target) * 0.5);
        prep.normals.push(normal);
        groups
            .entry((edge.source, edge.target))
            .or_default()
            .push(index);
    }
    let count = prep.edge_ids.len();
    prep.has_reverse.reserve(count);
    prep.parallel.reserve(count);
    for (index, id) in prep.edge_ids.iter().enumerate() {
        let edge = graph.edge(*id).expect("edge exists");
        let group = &groups[&(edge.source, edge.target)];
        prep.has_reverse
            .push(groups.contains_key(&(edge.target, edge.source)));
        if group.len() > 1 {
            let position = group.iter().position(|&i| i == index).unwrap_or(0);
            prep.parallel.push(Some((position, group.len())));
        } else {
            prep.parallel.push(None);
        }
    }
    prep.density_grid =
        DensityGrid::new_with_hasher(&prep.midpoints, crate::paint::DENSITY_RADIUS, hasher);
    prep
}

/// Derived rendering state for a graph scene (§20).
///
/// Holds the zoom-invariant graph preprocessing ([`EdgePrep`]) and a
/// uniform-grid spatial index over node positions and edge bounding boxes so
/// the paint layer can query only the primitives near the visible region
/// instead of scanning the whole graph every frame. All of it is rebuilt only
/// when the topology or geometry revision changes; pan/zoom reuse it.
///
/// Raw indexed queries are crate-private. External callers must obtain a
/// [`SyncedGraphRuntime`] from [`GraphScene::sync_runtime`] so the graph,
/// positions, revisions, and derived index are proven to describe one scene
/// snapshot:
///
/// ```compile_fail
/// use gpui_graph::GraphRuntime;
///
/// let runtime = GraphRuntime::new();
/// let _ = runtime.edges();
/// ```
///
/// ```compile_fail
/// use gpui_graph::{GraphRuntime, WorldBounds};
/// use glam::Vec2;
///
/// let runtime = GraphRuntime::new();
/// let bounds = WorldBounds { min: Vec2::ZERO, max: Vec2::ONE };
/// let _ = runtime.visible_edge_candidates(&bounds, 0.0);
/// ```
#[derive(Debug, Clone, Default)]
pub struct GraphRuntime<S = std::collections::hash_map::RandomState>
where
    S: BuildHasher + Default + Clone,
{
    /// Identity of the graph whose derived state is held below. `None` means
    /// the runtime has never been bound and is therefore always stale.
    source_identity: Option<Arc<()>>,
    /// The topology revision represented by this runtime.
    topology_revision: u64,
    /// The geometry revision represented by this runtime.
    geometry_revision: u64,
    /// Node ids bucketed by the grid cell of their position.
    node_cells: HashMap<Cell, Vec<NodeId>, S>,
    /// Edge candidate indices bucketed by every grid cell their curve bounding
    /// box (source, target, and control point) covers.
    edge_cells: HashMap<Cell, Vec<usize>, S>,
    /// Edge candidate indices whose bounding boxes cover more than the cell
    /// enumeration limit. They are checked on every normal query so oversized
    /// edges are never silently dropped.
    edge_overflow: Vec<usize>,
    /// The zoom-invariant per-edge preprocessing.
    edges: EdgePrep<S>,
}

/// An empty runtime using the default SipHash hasher.
impl GraphRuntime {
    /// Create an empty runtime.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S> GraphRuntime<S>
where
    S: BuildHasher + Default + Clone,
{
    /// The topology revision this runtime was synced to.
    pub fn topology_revision(&self) -> u64 {
        self.topology_revision
    }

    /// The geometry revision this runtime was synced to.
    pub fn geometry_revision(&self) -> u64 {
        self.geometry_revision
    }

    /// Whether the runtime is stale relative to one graph source and its
    /// revisions. An unbound runtime is always stale, even when the revision
    /// pair happens to be zero.
    pub(crate) fn is_stale_for<NK, EK, N, E>(
        &self,
        source: &RuntimeSource<'_, NK, EK, N, E, S>,
    ) -> bool {
        self.source_identity
            .as_ref()
            .is_none_or(|identity| !Arc::ptr_eq(identity, &source.source_identity))
            || self.topology_revision != source.topology_revision
            || self.geometry_revision != source.geometry_revision
    }

    /// Whether the derived state is bound to this exact graph source.
    ///
    /// Test-only introspection; production indexed rendering obtains the
    /// stronger source-and-revision proof from `GraphScene::sync_runtime`.
    #[cfg(test)]
    pub(crate) fn is_built_for<N, E>(&self, graph: &Graph<N, E>) -> bool {
        self.source_identity
            .as_ref()
            .is_some_and(|identity| Arc::ptr_eq(identity, &graph.source_identity()))
    }

    /// Construct and atomically install all derived state for `source`.
    ///
    /// Resolvers are evaluated while a replacement runtime is built off to the
    /// side. A panic or future fallible build cannot leave this runtime with a
    /// new source identity and old derived structures (or vice versa).
    pub(crate) fn rebuild_from_source<NK, EK, N, E>(
        &mut self,
        source: RuntimeSource<'_, NK, EK, N, E, S>,
    ) where
        NK: Eq + std::hash::Hash,
        EK: Eq + std::hash::Hash,
    {
        *self = Self::build_from_source(&source);
    }

    fn build_from_source<NK, EK, N, E>(source: &RuntimeSource<'_, NK, EK, N, E, S>) -> Self
    where
        NK: Eq + std::hash::Hash,
        EK: Eq + std::hash::Hash,
    {
        let scene = source.scene;
        let graph = scene.graph();
        let mut runtime = Self {
            source_identity: Some(Arc::clone(&source.source_identity)),
            topology_revision: source.topology_revision,
            geometry_revision: source.geometry_revision,
            ..Self::default()
        };
        runtime.edges = build_edge_prep(graph, &|id| scene.node_position(id), S::default());

        for (id, _) in graph.nodes() {
            let Some(pos) = scene.node_position(id) else {
                continue;
            };
            runtime.node_cells.entry(cell_of(pos)).or_default().push(id);
        }

        // Index each candidate edge's conservative curve bounding box for
        // spatial queries. The owner computes both capped density directions,
        // fan/cluster geometry, and bounded obstacle displacement, so the
        // index remains a superset of the final precise curve cull. An
        // oversized box goes to `edge_overflow` rather than enumerating an
        // unbounded number of cells.
        let empty_obstacle_grid =
            crate::paint::ObstacleGrid::new_with_hasher(&[], 1.0, S::default());
        for (index, (source_position, target_position)) in runtime
            .edges
            .source
            .iter()
            .zip(runtime.edges.target.iter())
            .enumerate()
        {
            let id = runtime.edges.edge_ids[index];
            let edge = graph.edge(id).expect("edge exists");
            // Self-loop geometry is defined in screen space from the current
            // viewport and fixed-pixel style. No finite world-space box can be
            // a viewport-independent superset, so every self-loop must reach
            // the precise screen-space cull through the bounded overflow list.
            if edge.source == edge.target {
                runtime.edge_overflow.push(index);
                continue;
            }
            let cluster = shared_cluster_center(edge.source, edge.target, &|node| {
                scene.node_cluster_center(node)
            });
            let (min, max) = edge_curve_bbox(
                *source_position,
                *target_position,
                index,
                &runtime.edges.has_reverse,
                &runtime.edges.parallel,
                cluster,
                &empty_obstacle_grid,
            );
            let Some(rect) = CellRect::from_world(min, max) else {
                runtime.edge_overflow.push(index);
                continue;
            };
            if !rect.within_limit() {
                runtime.edge_overflow.push(index);
                continue;
            }
            for cell in rect.cells() {
                runtime.edge_cells.entry(cell).or_default().push(index);
            }
        }
        runtime
    }

    /// Node ids whose position may fall within `bounds` expanded by `margin`.
    ///
    /// This is a coarse pre-filter: a node in a boundary cell may lie just
    /// outside the rect, so the caller must still apply the precise
    /// point-in-bounds test. Returns a superset of the nodes the paint layer
    /// would keep.
    pub(crate) fn visible_nodes(&self, bounds: &WorldBounds, margin: f32) -> Vec<NodeId> {
        let Some(rect) = CellRect::from_query(bounds, margin) else {
            return self.node_cells.values().flatten().copied().collect();
        };
        if !rect.within_limit() {
            return self.node_cells.values().flatten().copied().collect();
        }
        rect.cells()
            .filter_map(|cell| self.node_cells.get(&cell))
            .flat_map(|ids| ids.iter().copied())
            .collect()
    }

    /// Edge candidate indices whose curve bounding box may intersect `bounds`
    /// expanded by `margin` plus [`EDGE_INDEX_SLACK`].
    ///
    /// This is a coarse pre-filter: the caller must still run the precise
    /// curve-bounding-box test on each candidate. The slack guarantees the
    /// candidate set is a superset of the edges the precise test would keep,
    /// so the visible set is unchanged. An edge's bounding box covers multiple
    /// cells, so the result is deduplicated. The returned indices index into
    /// [`Self::edges`].
    pub(crate) fn visible_edge_candidates(&self, bounds: &WorldBounds, margin: f32) -> Vec<usize> {
        let mut result = Vec::new();
        let mut seen: HashSet<usize, S> = HashSet::with_hasher(S::default());
        let slack = margin + EDGE_INDEX_SLACK;
        let Some(rect) = CellRect::from_query(bounds, slack) else {
            return (0..self.edges.edge_ids.len()).collect();
        };
        if !rect.within_limit() {
            return (0..self.edges.edge_ids.len()).collect();
        }
        for cell in rect.cells() {
            if let Some(ids) = self.edge_cells.get(&cell) {
                for &id in ids {
                    if seen.insert(id) {
                        result.push(id);
                    }
                }
            }
        }
        // Oversized edge boxes are always included in the normal query. They
        // are a deliberate candidate superset; the precise curve test filters
        // them without requiring unbounded bucket population.
        for &id in &self.edge_overflow {
            if seen.insert(id) {
                result.push(id);
            }
        }
        result
    }

    /// The zoom-invariant per-edge preprocessing.
    pub(crate) fn edges(&self) -> &EdgePrep<S> {
        &self.edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeDirection;
    use crate::layout::{FixedLayout, SccLayoutEngine};
    use crate::patch::GraphBatch;
    use crate::scene::GraphScene;

    struct Sample {
        scene: GraphScene<String, String, (), ()>,
        nodes: [NodeId; 4],
    }

    fn sample() -> Sample {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("a".to_owned(), ())
                .node("b".to_owned(), ())
                .node("c".to_owned(), ())
                .node("d".to_owned(), ())
                .edge(
                    "ab".to_owned(),
                    "a".to_owned(),
                    "b".to_owned(),
                    EdgeDirection::Directed,
                    (),
                )
                .edge(
                    "bc".to_owned(),
                    "b".to_owned(),
                    "c".to_owned(),
                    EdgeDirection::Directed,
                    (),
                )
                // A long edge from a to d whose bounding box spans the whole
                // graph.
                .edge(
                    "ad".to_owned(),
                    "a".to_owned(),
                    "d".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let nodes = ["a", "b", "c", "d"]
            .map(|key| scene.node_id(&key.to_owned()).expect("sample node exists"));
        for (node, position) in nodes.iter().zip([
            glam::Vec2::new(0.0, 0.0),
            glam::Vec2::new(100.0, 0.0),
            glam::Vec2::new(0.0, 100.0),
            glam::Vec2::new(1000.0, 1000.0),
        ]) {
            scene.set_position(*node, position);
        }
        Sample { scene, nodes }
    }

    fn sync(scene: &GraphScene<String, String, (), ()>) -> GraphRuntime {
        let mut runtime = GraphRuntime::new();
        scene.sync_runtime(&mut runtime);
        runtime
    }

    fn bounds_contains_point(bounds: &WorldBounds, point: glam::Vec2) -> bool {
        point.x >= bounds.min.x
            && point.x <= bounds.max.x
            && point.y >= bounds.min.y
            && point.y <= bounds.max.y
    }

    #[test]
    fn rebuild_populates_index_and_edge_prep() {
        let s = sample();
        let rt = sync(&s.scene);
        assert!(!rt.node_cells.is_empty());
        assert!(!rt.edge_cells.is_empty());
        // The edge prep holds one entry per candidate edge.
        assert_eq!(rt.edges().edge_ids.len(), 3);
        assert_eq!(rt.edges().midpoints.len(), 3);
        assert_eq!(rt.edges().normals.len(), 3);
        assert_eq!(rt.edges().has_reverse.len(), 3);
        assert_eq!(rt.edges().parallel.len(), 3);
    }

    #[test]
    fn visible_nodes_returns_nodes_in_rect() {
        let s = sample();
        let rt = sync(&s.scene);
        let bounds = WorldBounds {
            min: glam::Vec2::new(-10.0, -10.0),
            max: glam::Vec2::new(110.0, 110.0),
        };
        let nodes = rt.visible_nodes(&bounds, 0.0);
        let expected = s
            .scene
            .graph()
            .nodes()
            .filter_map(|(id, _)| {
                let position = s.scene.node_position(id).expect("sample position");
                (position.x <= 110.0 && position.y <= 110.0).then_some(id)
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            nodes.into_iter().collect::<std::collections::HashSet<_>>(),
            expected
        );
    }

    #[test]
    fn visible_edge_candidates_covers_curve_bbox_with_endpoints_outside_view() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("source".to_owned(), ())
                .node("target".to_owned(), ())
                .edge(
                    "long".to_owned(),
                    "source".to_owned(),
                    "target".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let long_edge = scene.edge_id(&"long".to_owned()).unwrap();
        scene.set_position(source, glam::Vec2::new(-1_000.0, 0.0));
        scene.set_position(target, glam::Vec2::new(1_000.0, 0.0));
        let rt = sync(&scene);
        // Both endpoints are outside this view, but the edge's curve bounding
        // box crosses it at the midpoint. This must not be reduced to an
        // endpoint or midpoint-only query.
        let bounds = WorldBounds {
            min: glam::Vec2::new(-10.0, -10.0),
            max: glam::Vec2::new(10.0, 10.0),
        };
        assert!(!bounds_contains_point(
            &bounds,
            scene.node_position(source).unwrap()
        ));
        assert!(!bounds_contains_point(
            &bounds,
            scene.node_position(target).unwrap()
        ));
        let candidates = rt.visible_edge_candidates(&bounds, 0.0);
        let candidate_ids = candidates
            .iter()
            .map(|&index| rt.edges().edge_ids[index])
            .collect::<Vec<_>>();
        assert_eq!(candidate_ids, vec![long_edge]);
    }

    #[test]
    fn sync_records_scene_revisions_with_derived_state() {
        let s = sample();
        let rt = sync(&s.scene);
        assert_eq!(rt.topology_revision(), s.scene.topology_revision());
        assert_eq!(rt.geometry_revision(), s.scene.geometry_revision());
        assert!(rt.is_built_for(s.scene.graph()));
    }

    #[test]
    fn index_covers_parallel_fanned_control_point() {
        // A parallel group of many edges fans the control point far outside the
        // source-target bounding box. The index must cover that control point so
        // a view near it still returns the edge as a candidate (the precise cull
        // test would keep it). This reproduces the "edges disappear at high zoom"
        // bug: the old index bucketed only the source-target box, so a fanned
        // edge whose control point was outside the box (and beyond the fixed
        // slack) was dropped.
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        let mut batch = GraphBatch::new()
            .node("a".to_owned(), ())
            .node("b".to_owned(), ());
        for _ in 0..8 {
            let key = format!("e{}", batch.edges.len());
            batch = batch.edge(
                key,
                "a".to_owned(),
                "b".to_owned(),
                EdgeDirection::Directed,
                (),
            );
        }
        scene.merge(batch);
        let a = scene.node_id(&"a".to_owned()).unwrap();
        let b = scene.node_id(&"b".to_owned()).unwrap();
        scene.set_position(a, glam::Vec2::new(0.0, 0.0));
        scene.set_position(b, glam::Vec2::new(100.0, 0.0));
        let parallel_edge_ids = (0..8)
            .map(|i| scene.edge_id(&format!("e{i}")).unwrap())
            .collect::<Vec<_>>();
        let rt = sync(&scene);

        // The outermost fanned edge's control point sits at
        // (50, (8-1)/2 * spacing). With 8 edges the fan offset is large; query a
        // view that contains only the control point, far from the source-target
        // box. The edge must still be a candidate.
        let bounds = WorldBounds {
            min: glam::Vec2::new(40.0, 100.0),
            max: glam::Vec2::new(60.0, 200.0),
        };
        let edges = rt.visible_edge_candidates(&bounds, 0.0);
        let candidate_ids = edges
            .iter()
            .map(|&index| rt.edges().edge_ids[index])
            .collect::<Vec<_>>();
        let outer_edge = *parallel_edge_ids
            .last()
            .expect("parallel group is non-empty");
        let outer_index = rt
            .edges()
            .edge_ids
            .iter()
            .position(|id| *id == outer_edge)
            .expect("outer edge is preprocessed");
        let empty_obstacles = crate::paint::ObstacleGrid::new(&[], 1.0);
        let control = crate::paint::edge_control_point(
            rt.edges().source[outer_index],
            rt.edges().target[outer_index],
            &crate::paint::EdgeCurveContext {
                index: outer_index,
                signed_density: 0.0,
                has_reverse: &rt.edges().has_reverse,
                parallel: &rt.edges().parallel,
                obstacles: &empty_obstacles,
                node_radius: 0.0,
            },
            None,
        );
        let (bbox_min, bbox_max) = edge_curve_bbox(
            rt.edges().source[outer_index],
            rt.edges().target[outer_index],
            outer_index,
            &rt.edges().has_reverse,
            &rt.edges().parallel,
            None,
            &empty_obstacles,
        );
        assert!(bounds_contains_point(
            &WorldBounds {
                min: bbox_min,
                max: bbox_max,
            },
            control
        ));
        assert!(
            rt.edge_cells
                .get(&cell_of(control))
                .is_some_and(|indices| indices.contains(&outer_index)),
            "the stored index cell must directly cover the parallel control point"
        );
        assert!(candidate_ids.contains(&outer_edge));
        assert!(
            !edges.is_empty(),
            "fanned edge must remain a candidate near its control point"
        );
    }

    #[test]
    fn index_covers_cluster_bowed_control_point() {
        // A cluster-bowed edge pushes its control point outside the source-target
        // box by up to ~1.25x the cluster radius. The index must cover it so a
        // view near the control point still returns the edge.
        let mut scene = GraphScene::new().with_layout(Box::new(SccLayoutEngine));
        scene.merge(
            GraphBatch::new()
                .node("a".to_owned(), ())
                .node("b".to_owned(), ())
                .edge(
                    "ab".to_owned(),
                    "a".to_owned(),
                    "b".to_owned(),
                    EdgeDirection::Directed,
                    (),
                )
                .edge(
                    "ba".to_owned(),
                    "b".to_owned(),
                    "a".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let expected_edge = scene.edge_id(&"ab".to_owned()).unwrap();
        let rt = sync(&scene);
        let expected_index = rt
            .edges()
            .edge_ids
            .iter()
            .position(|id| *id == expected_edge)
            .expect("cluster edge is preprocessed");
        let edge = scene.graph().edge(expected_edge).unwrap();
        let cluster = shared_cluster_center(edge.source, edge.target, &|node| {
            scene.node_cluster_center(node)
        });
        assert!(
            cluster.is_some(),
            "fixture must produce shared SCC geometry"
        );
        let empty_obstacles = crate::paint::ObstacleGrid::new(&[], 1.0);
        let control = crate::paint::edge_control_point(
            rt.edges().source[expected_index],
            rt.edges().target[expected_index],
            &crate::paint::EdgeCurveContext {
                index: expected_index,
                signed_density: 0.0,
                has_reverse: &rt.edges().has_reverse,
                parallel: &rt.edges().parallel,
                obstacles: &empty_obstacles,
                node_radius: 0.0,
            },
            cluster,
        );
        let (bbox_min, bbox_max) = edge_curve_bbox(
            rt.edges().source[expected_index],
            rt.edges().target[expected_index],
            expected_index,
            &rt.edges().has_reverse,
            &rt.edges().parallel,
            cluster,
            &empty_obstacles,
        );
        assert!(bounds_contains_point(
            &WorldBounds {
                min: bbox_min,
                max: bbox_max,
            },
            control
        ));
        assert!(
            rt.edge_cells
                .get(&cell_of(control))
                .is_some_and(|indices| indices.contains(&expected_index)),
            "the stored index cell must directly cover the cluster control point"
        );

        // The control point bows outward from the center, beyond the chord. Query
        // a view that contains only the outward-bowed region, outside the
        // source-target box. The edge must still be a candidate.
        let bounds = WorldBounds {
            min: glam::Vec2::new(0.0, 0.0),
            max: glam::Vec2::new(200.0, 200.0),
        };
        let edges = rt.visible_edge_candidates(&bounds, 0.0);
        let edge_ids = edges
            .iter()
            .map(|&index| rt.edges().edge_ids[index])
            .collect::<Vec<_>>();
        assert!(edge_ids.contains(&expected_edge));
        assert!(
            !edges.is_empty(),
            "cluster-bowed edge must remain a candidate near its control point"
        );
    }

    #[test]
    fn coincident_distinct_nodes_are_not_treated_as_self_loop_overflow() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("source".to_owned(), ())
                .node("target".to_owned(), ())
                .edge(
                    "coincident".to_owned(),
                    "source".to_owned(),
                    "target".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let edge_id = scene.edge_id(&"coincident".to_owned()).unwrap();
        let position = glam::Vec2::new(0.0, 2_250.0);
        scene.set_position(source, position);
        scene.set_position(target, position);

        let mut runtime = GraphRuntime::new();
        let edge_index = {
            let synced = scene.sync_runtime(&mut runtime);
            synced
                .edges()
                .edge_ids
                .iter()
                .position(|id| *id == edge_id)
                .expect("coincident edge is preprocessed")
        };
        assert!(
            !runtime.edge_overflow.contains(&edge_index),
            "only logical self-loops may use the viewport-independent overflow path"
        );

        let synced = scene.sync_runtime(&mut runtime);

        // This query is more than EDGE_INDEX_SLACK away from the degenerate
        // non-loop bbox. A position-based self-loop classification would place
        // the edge in overflow and return it here regardless of its bbox.
        let far_bounds = WorldBounds {
            min: glam::Vec2::new(-1_000.0, -1_000.0),
            max: glam::Vec2::new(1_000.0, 1_000.0),
        };
        let far_candidates = synced.visible_edge_candidates(&far_bounds, 0.0);
        assert!(
            far_candidates.is_empty(),
            "coincident distinct nodes must not leak a self-loop candidate beyond index slack: {far_candidates:?}"
        );

        // The near query reaches the endpoint bbox, proving the edge is still
        // indexed as a non-loop candidate rather than discarded.
        let near_bounds = WorldBounds {
            min: glam::Vec2::new(-10.0, 2_240.0),
            max: glam::Vec2::new(10.0, 2_260.0),
        };
        assert!(
            synced
                .visible_edge_candidates(&near_bounds, 0.0)
                .contains(&edge_index),
            "the non-loop candidate bbox must remain queryable near its control bound"
        );

        let mut viewport = crate::viewport::Viewport::new();
        viewport.set_size(glam::Vec2::new(100.0, 100.0));
        viewport.fit_bounds(
            WorldBounds {
                min: glam::Vec2::new(-2_000.0, -2_000.0),
                max: glam::Vec2::new(2_000.0, 2_000.0),
            },
            0.0,
        );
        assert!((viewport.zoom() - 0.025).abs() < f32::EPSILON);
        let visible_world = viewport.visible_world_bounds();
        assert!(
            position.x < visible_world.min.x
                || position.x > visible_world.max.x
                || position.y < visible_world.min.y
                || position.y > visible_world.max.y
        );
        assert!(
            position.y > visible_world.max.y + EDGE_INDEX_SLACK,
            "the coincident endpoint must be beyond the indexed query slack"
        );
        let graph = scene.graph();
        let positions = |id: NodeId| scene.node_position(id);
        let style = crate::style::GraphStyle::default();
        let node_screen = viewport.world_to_screen(position);
        assert!(
            node_screen.y > viewport.size().y,
            "the endpoint must be outside the low-zoom screen viewport"
        );
        // Before topology-aware classification, the cull treated coincident
        // coordinates as a self-loop. The old path used the correct screen
        // position, so this onigiri enters the canvas even though the endpoint
        // bbox is far beyond index slack.
        let old_position_loop =
            crate::paint::self_loop_path(source, node_screen, graph, &positions, &viewport, &style);
        let screen_bounds = WorldBounds {
            min: glam::Vec2::ZERO,
            max: viewport.size(),
        };
        let screen_box_intersects = |min: glam::Vec2, max: glam::Vec2| {
            min.x <= screen_bounds.max.x
                && max.x >= screen_bounds.min.x
                && min.y <= screen_bounds.max.y
                && max.y >= screen_bounds.min.y
        };
        assert!(
            old_position_loop.iter().any(|(p0, p1, p2)| {
                screen_box_intersects(p0.min(*p1).min(*p2), p0.max(*p1).max(*p2))
            }),
            "the old position-based onigiri would enter this non-unit-zoom view"
        );
        let selection = crate::interaction::Selection::new();
        let hover = crate::interaction::Hover::default();
        let linear = crate::paint::build_paint_frame(crate::paint::PaintFrameInput {
            graph,
            node_position: &positions,
            node_cluster_center: &|id| scene.node_cluster_center(id),
            node_label: &|_, _| None,
            edge_label: &|_, _| None,
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
        });
        let frame = crate::paint::build_indexed_paint_frame(crate::paint::IndexedPaintFrameInput {
            synced: &synced,
            node_label: &|_, _| None,
            edge_label: &|_, _| None,
            viewport: &viewport,
            style: &style,
            selection: &selection,
            hover: &hover,
        });
        let linear_ids = linear.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        let indexed_ids = frame.edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
        assert_eq!(linear_ids, Vec::<EdgeId>::new());
        assert_eq!(indexed_ids, Vec::<EdgeId>::new());
        assert_eq!(indexed_ids, linear_ids);
    }

    #[test]
    fn is_stale_detects_revision_mismatch() {
        let mut s = sample();
        let mut rt = sync(&s.scene);
        let topology = rt.topology_revision();
        let geometry = rt.geometry_revision();
        s.scene.set_position(s.nodes[0], glam::Vec2::new(1.0, 2.0));
        assert_eq!(s.scene.topology_revision(), topology);
        assert_ne!(s.scene.geometry_revision(), geometry);
        assert!(rt.geometry_revision() != s.scene.geometry_revision());
        s.scene.sync_runtime(&mut rt);
        assert_eq!(rt.geometry_revision(), s.scene.geometry_revision());
    }

    #[test]
    fn source_identity_rejects_same_revisions_from_another_graph() {
        let mut first = sample();
        let mut second = sample();
        first
            .scene
            .set_position(first.nodes[0], glam::Vec2::new(-10_000.0, 0.0));
        second
            .scene
            .set_position(second.nodes[0], glam::Vec2::new(10_000.0, 0.0));
        let mut runtime = sync(&first.scene);
        assert_eq!(
            first.scene.topology_revision(),
            second.scene.topology_revision()
        );
        assert_eq!(
            first.scene.geometry_revision(),
            second.scene.geometry_revision()
        );
        assert!(!runtime.is_built_for(second.scene.graph()));
        assert!(runtime.is_built_for(first.scene.graph()));
        second.scene.sync_runtime(&mut runtime);
        assert!(runtime.is_built_for(second.scene.graph()));
        assert_eq!(
            runtime.edges().source[0],
            second.scene.node_position(second.nodes[0]).unwrap()
        );
    }

    #[test]
    fn graph_clone_gets_a_distinct_runtime_source_identity() {
        let original = sample();
        let mut cloned_graph = original.scene.graph().clone();

        let original_nodes = original
            .scene
            .graph()
            .nodes()
            .map(|(id, _)| {
                (
                    id,
                    original.scene.graph().incident_edges(id).unwrap().to_vec(),
                )
            })
            .collect::<Vec<_>>();
        let cloned_nodes = cloned_graph
            .nodes()
            .map(|(id, _)| (id, cloned_graph.incident_edges(id).unwrap().to_vec()))
            .collect::<Vec<_>>();
        assert_eq!(cloned_nodes, original_nodes);
        assert_eq!(
            cloned_graph.node_count(),
            original.scene.graph().node_count()
        );
        assert_eq!(
            cloned_graph.edge_count(),
            original.scene.graph().edge_count()
        );

        let original_edges = original
            .scene
            .graph()
            .edges()
            .map(|(id, edge)| (id, edge.source, edge.target, edge.direction, edge.data))
            .collect::<Vec<_>>();
        let cloned_edges = cloned_graph
            .edges()
            .map(|(id, edge)| (id, edge.source, edge.target, edge.direction, edge.data))
            .collect::<Vec<_>>();
        assert_eq!(cloned_edges, original_edges);

        let runtime = sync(&original.scene);
        assert_eq!(
            runtime.topology_revision(),
            original.scene.topology_revision()
        );
        assert_eq!(
            runtime.geometry_revision(),
            original.scene.geometry_revision()
        );
        assert!(runtime.is_built_for(original.scene.graph()));
        assert!(!runtime.is_built_for(&cloned_graph));

        let original_node_count = original.scene.graph().node_count();
        let original_edge_count = original.scene.graph().edge_count();
        let original_first_node = original
            .scene
            .graph()
            .nodes()
            .next()
            .expect("sample has a node")
            .0;
        assert_eq!(
            original.scene.graph().node_data(original_first_node),
            Some(&())
        );

        let clone_only_node = cloned_graph.add_node(());
        cloned_graph
            .add_edge(
                original_first_node,
                clone_only_node,
                EdgeDirection::Directed,
                (),
            )
            .expect("cloned endpoints exist");

        assert_eq!(original.scene.graph().node_count(), original_node_count);
        assert_eq!(original.scene.graph().edge_count(), original_edge_count);
        assert_eq!(
            original.scene.graph().node_data(original_first_node),
            Some(&())
        );
        assert_eq!(
            original
                .scene
                .graph()
                .nodes()
                .map(|(id, _)| {
                    (
                        id,
                        original.scene.graph().incident_edges(id).unwrap().to_vec(),
                    )
                })
                .collect::<Vec<_>>(),
            original_nodes
        );
        assert_eq!(
            original
                .scene
                .graph()
                .edges()
                .map(|(id, edge)| (id, edge.source, edge.target, edge.direction, edge.data))
                .collect::<Vec<_>>(),
            original_edges
        );
        assert_eq!(cloned_graph.node_count(), original_node_count + 1);
        assert_eq!(cloned_graph.edge_count(), original_edge_count + 1);
        assert!(runtime.is_built_for(original.scene.graph()));
        assert_eq!(
            runtime.topology_revision(),
            original.scene.topology_revision()
        );
        assert_eq!(
            runtime.geometry_revision(),
            original.scene.geometry_revision()
        );
        assert!(!runtime.is_built_for(&cloned_graph));
    }

    #[test]
    fn source_rebuild_replaces_derived_state_and_revisions_atomically() {
        let first = sample();
        let mut second: GraphScene<String, String, (), ()> =
            GraphScene::new().with_layout(Box::new(FixedLayout));
        second.merge(GraphBatch::new().node("only".to_owned(), ()));
        let node = second.node_id(&"only".to_owned()).unwrap();
        second.set_position(node, glam::Vec2::new(4_000.0, 4_000.0));
        let mut runtime = sync(&first.scene);
        second.sync_runtime(&mut runtime);

        assert_eq!(runtime.topology_revision(), second.topology_revision());
        assert_eq!(runtime.geometry_revision(), second.geometry_revision());
        assert_eq!(runtime.edges().edge_ids, Vec::<EdgeId>::new());
        assert_eq!(
            runtime.visible_nodes(
                &WorldBounds {
                    min: glam::Vec2::new(3_900.0, 3_900.0),
                    max: glam::Vec2::new(4_100.0, 4_100.0),
                },
                0.0
            ),
            vec![node]
        );
        assert!(runtime.is_built_for(second.graph()));
    }

    #[test]
    fn huge_query_falls_back_to_all_indexed_primitives() {
        let sample = sample();
        let runtime = sync(&sample.scene);
        let bounds = WorldBounds {
            min: glam::Vec2::splat(-f32::MAX),
            max: glam::Vec2::splat(f32::MAX),
        };
        let nodes = runtime.visible_nodes(&bounds, 0.0);
        assert_eq!(nodes.len(), sample.scene.graph().node_count());
        let edges = runtime.visible_edge_candidates(&bounds, 0.0);
        assert_eq!(edges.len(), runtime.edges().edge_ids.len());
    }

    #[test]
    fn oversized_edge_bbox_uses_overflow_and_remains_a_candidate() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(
            GraphBatch::new()
                .node("source".to_owned(), ())
                .node("target".to_owned(), ())
                .edge(
                    "long".to_owned(),
                    "source".to_owned(),
                    "target".to_owned(),
                    EdgeDirection::Directed,
                    (),
                ),
        );
        let source = scene.node_id(&"source".to_owned()).unwrap();
        let target = scene.node_id(&"target".to_owned()).unwrap();
        let expected = scene.edge_id(&"long".to_owned()).unwrap();
        scene.set_position(source, glam::Vec2::new(-1_000_000.0, 0.0));
        scene.set_position(target, glam::Vec2::new(1_000_000.0, 0.0));
        let runtime = sync(&scene);
        assert!(runtime.edge_cells.is_empty());
        assert_eq!(runtime.edge_overflow, vec![0]);
        let candidates = runtime.visible_edge_candidates(
            &WorldBounds {
                min: glam::Vec2::new(-10.0, -10.0),
                max: glam::Vec2::new(10.0, 10.0),
            },
            0.0,
        );
        assert_eq!(
            candidates
                .into_iter()
                .map(|index| runtime.edges().edge_ids[index])
                .collect::<Vec<_>>(),
            vec![expected]
        );
    }

    #[test]
    fn self_loop_always_uses_overflow_instead_of_a_fixed_world_bbox() {
        let mut scene = GraphScene::new().with_layout(Box::new(FixedLayout));
        scene.merge(GraphBatch::new().node("node".to_owned(), ()).edge(
            "loop".to_owned(),
            "node".to_owned(),
            "node".to_owned(),
            EdgeDirection::Directed,
            (),
        ));
        let node = scene.node_id(&"node".to_owned()).unwrap();
        let expected = scene.edge_id(&"loop".to_owned()).unwrap();
        scene.set_position(node, glam::Vec2::new(0.0, 10_000.0));
        let runtime = sync(&scene);
        let index = runtime
            .edges()
            .edge_ids
            .iter()
            .position(|id| *id == expected)
            .unwrap();
        assert_eq!(runtime.edge_overflow, vec![index]);
        assert!(
            runtime
                .edge_cells
                .values()
                .all(|indices| !indices.contains(&index))
        );
        let candidate_ids = runtime
            .visible_edge_candidates(
                &WorldBounds {
                    min: glam::Vec2::new(-10.0, -10.0),
                    max: glam::Vec2::new(10.0, 10.0),
                },
                0.0,
            )
            .into_iter()
            .map(|candidate| runtime.edges().edge_ids[candidate])
            .collect::<Vec<_>>();
        assert_eq!(candidate_ids, vec![expected]);
    }

    #[test]
    fn cell_limit_is_checked_at_the_boundary() {
        let boundary = CellRect {
            min: (0, 0),
            max: (63, 63),
        };
        assert_eq!(boundary.cell_count(), Some(MAX_INDEX_CELLS));
        assert!(boundary.within_limit());
        let over = CellRect {
            min: (0, 0),
            max: (64, 63),
        };
        assert_eq!(over.cell_count(), Some(MAX_INDEX_CELLS + 64));
        assert!(!over.within_limit());
    }

    #[test]
    fn density_grid_keeps_neighbors_at_saturated_cell_boundaries() {
        let radius = crate::paint::DENSITY_RADIUS;
        let world_cell_limit = (i32::MAX as f32) * radius * 2.0;
        let cases = [
            (
                "positive x",
                glam::Vec2::new(world_cell_limit, 0.0),
                glam::Vec2::new(world_cell_limit, radius * 0.5),
            ),
            (
                "negative x",
                glam::Vec2::new(-world_cell_limit, 0.0),
                glam::Vec2::new(-world_cell_limit, radius * 0.5),
            ),
            (
                "positive y",
                glam::Vec2::new(0.0, world_cell_limit),
                glam::Vec2::new(radius * 0.5, world_cell_limit),
            ),
            (
                "negative y",
                glam::Vec2::new(0.0, -world_cell_limit),
                glam::Vec2::new(radius * 0.5, -world_cell_limit),
            ),
        ];

        for (name, midpoint, neighbor) in cases {
            let grid = DensityGrid::new(&[midpoint, neighbor], radius);
            let candidates = grid.candidates(midpoint, radius).collect::<Vec<_>>();
            assert_eq!(
                candidates,
                vec![0, 1],
                "{name} query must visit each saturated boundary cell exactly once"
            );

            let densities = crate::paint::signed_densities_for(
                &grid,
                &[midpoint, neighbor],
                &[
                    if (neighbor.x - midpoint.x).abs() > f32::EPSILON {
                        glam::Vec2::X
                    } else {
                        glam::Vec2::Y
                    },
                    if (neighbor.x - midpoint.x).abs() > f32::EPSILON {
                        glam::Vec2::X
                    } else {
                        glam::Vec2::Y
                    },
                ],
                radius,
                &[0, 1],
            );
            assert!(
                (densities[0] - 0.5).abs() < 1e-5,
                "{name} signed density must count the neighbor once: {:?}",
                densities
            );
            assert!(
                (densities[1] + 0.5).abs() < 1e-5,
                "{name} reverse signed density must preserve orientation: {:?}",
                densities
            );
        }
    }
}
