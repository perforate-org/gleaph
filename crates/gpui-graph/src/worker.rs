//! Worker backend protocol and host side (§18.2, ADR 0076 S2).
//!
//! When a view selects [`FrameSource::Worker`](crate::FrameSource), a dedicated
//! worker owns a graph-backend replica: it receives scene mutations and
//! interaction-state snapshots over this module's protocol, steps the layout,
//! builds the indexed paint frame, and ships [`PaintFrameWire`] buffers back
//! to the main thread. The main thread keeps input handling and painting; it
//! renders the last delivered frame until the worker replaces it.
//!
//! Layering:
//!
//! - This module is platform-independent and fully native-testable: message
//!   vocabulary ([`ToWorker`] / [`FromWorker`]), the worker-side inbox whose
//!   latest-wins slot owns backpressure ([`WorkerInbox`]), the backend loop
//!   ([`WorkerBackend`]), the byte forms of library-owned request content,
//!   and the replay/envelope state machine of the application channel
//!   ([`pipe_core`] with its [`PayloadCodec`](pipe_core::PayloadCodec) seam).
//! - [`web_transport`] (wasm only) is the postMessage glue around that core,
//!   including the main-thread [`PostMessageChannel`](web_transport::PostMessageChannel)
//!   and the worker-side [`serve`](web_transport::serve) loop; decisions about
//!   application payload bytes stay with the application.
//!
//! Backpressure contract (ADR 0076 §3): the worker keeps only the latest
//! interaction state, dropping intermediate snapshots, while scene mutations
//! queue FIFO and are never dropped — dropping a batch would lose entities,
//! whereas an older snapshot carries no information the newer one lacks.

#[cfg(target_family = "wasm")]
pub mod web_transport;

mod pipe_core;

pub use pipe_core::{Inbound, decode_inbound, encode_request};
pub use pipe_core::{PayloadCodec, READY, envelope};

use std::collections::VecDeque;

use glam::Vec2;
use slotmap::Key;

use crate::frame_source::{PaintFrameWire, WireFormatError};
use crate::graph::{EdgeId, NodeId};
use crate::hash::DefaultBuildHasher;
use crate::interaction::{Hover, Selection};
use crate::layout::LayoutBudget;
use crate::paint::{IndexedPaintFrameInput, OverlayCategory, build_indexed_paint_frame};
use crate::patch::{GraphBatch, GraphPatch};
use crate::runtime::GraphRuntime;
use crate::scene::GraphScene;
use crate::style::GraphStyle;
use crate::viewport::Viewport;

/// Everything the worker needs besides queued mutations to build one frame.
///
/// A full snapshot rather than per-field updates: the frame builder consumes
/// exactly this set, so replacing the pending snapshot wholesale is what makes
/// latest-wins loss safe.
#[derive(Debug, Clone)]
pub struct FrameState {
    /// The camera for the coming frame.
    pub viewport: Viewport,
    /// The style the coming frame is built with.
    pub style: GraphStyle,
    /// Current selection.
    pub selection: Selection,
    /// Current hover target.
    pub hover: Hover,
}

/// One ordered scene change for the worker-owned replica.
#[derive(Debug)]
pub enum SceneMutation<NK, EK, N, E> {
    /// Merge a batch of graph data ([`GraphScene::merge`]).
    Merge(GraphBatch<NK, EK, N, E>),
    /// Apply explicit mutations ([`GraphScene::apply`]).
    Apply(GraphPatch<NK, EK, N, E>),
    /// Move one node (e.g. a drag edit).
    SetPosition {
        /// The node to move.
        node: NodeId,
        /// Its new world-space position.
        position: Vec2,
    },
}

/// A main-thread request toward the worker backend.
#[derive(Debug)]
pub enum ToWorker<NK, EK, N, E> {
    /// An ordered scene change; queued FIFO, never dropped.
    Mutation(SceneMutation<NK, EK, N, E>),
    /// A replaceable interaction-state snapshot; latest-wins. Boxed because
    /// snapshots are far larger than the move variant and requests travel
    /// one at a time.
    FrameState(Box<FrameState>),
}

/// A worker-to-main-thread response.
#[derive(Debug)]
pub enum FromWorker {
    /// A finished frame. Its transfer form is exactly
    /// [`PaintFrameWire::to_wire_bytes`]; direction context distinguishes it
    /// from requests, so no extra framing byte is spent.
    Frame(PaintFrameWire),
}

/// Main-thread half of the connection to a worker backend.
///
/// Implementations forward requests over their transport (a real web Worker
/// on wasm; any harness when testing). Frames travel back through
/// [`crate::view::GraphViewState::deliver_worker_frame`], which transports
/// typically invoke from their message handler.
pub trait WorkerChannel<NK, EK, N, E> {
    /// Forward one request toward the worker backend.
    fn post(&mut self, request: ToWorker<NK, EK, N, E>);
}

/// The worker-side request inbox: one FIFO mutation queue plus one
/// latest-wins frame-state slot.
///
/// This type owns the backpressure semantics of the whole protocol, so both
/// order classes are observable separately through [`Self::drain`].
#[derive(Debug)]
pub struct WorkerInbox<NK, EK, N, E> {
    mutations: VecDeque<SceneMutation<NK, EK, N, E>>,
    frame_state: Option<FrameState>,
}

impl<NK, EK, N, E> Default for WorkerInbox<NK, EK, N, E> {
    fn default() -> Self {
        Self {
            mutations: VecDeque::new(),
            frame_state: None,
        }
    }
}

/// Everything removed from a [`WorkerInbox`] by one [`WorkerInbox::drain`].
#[derive(Debug)]
pub struct InboxDrain<NK, EK, N, E> {
    /// Queued mutations, in posting order.
    pub mutations: Vec<SceneMutation<NK, EK, N, E>>,
    /// The newest pending frame state, if one was posted since the last drain.
    pub frame_state: Option<FrameState>,
}

impl<NK, EK, N, E> WorkerInbox<NK, EK, N, E> {
    /// Create an empty inbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue one main-thread request.
    ///
    /// Mutations append to the FIFO queue; a frame state replaces whatever
    /// snapshot was pending. Intermediate snapshots are dropped by
    /// replacement, never stored — that replacement is the entire
    /// backpressure mechanism.
    pub fn post(&mut self, request: ToWorker<NK, EK, N, E>) {
        match request {
            ToWorker::Mutation(mutation) => self.mutations.push_back(mutation),
            ToWorker::FrameState(frame_state) => self.frame_state = Some(*frame_state),
        }
    }

    /// Remove everything queued: mutations in posting order, then the newest
    /// pending frame state if any. After a drain the inbox is empty.
    pub fn drain(&mut self) -> InboxDrain<NK, EK, N, E> {
        InboxDrain {
            mutations: std::mem::take(&mut self.mutations).into_iter().collect(),
            frame_state: self.frame_state.take(),
        }
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.mutations.is_empty() && self.frame_state.is_none()
    }
}

/// Label text resolver for one node, or `None` for no label.
pub type NodeLabelFn<N> = fn(NodeId, &N) -> Option<String>;
/// Label text resolver for one edge, or `None` for no label.
pub type EdgeLabelFn<E> = fn(EdgeId, &E) -> Option<String>;
/// Query-overlay category resolver for one node.
pub type NodeOverlayFn = fn(NodeId) -> OverlayCategory;
/// Query-overlay category resolver for one edge.
pub type EdgeOverlayFn = fn(EdgeId) -> OverlayCategory;

/// The worker-owned graph backend (ADR 0076 S2).
///
/// Owns the replica scene, its derived runtime, and the request inbox.
/// [`Self::step`] is the whole worker cycle: drain the inbox (mutations FIFO,
/// then the newest snapshot), apply the mutations, spend one layout step, and
/// build one frame from that snapshot. Plain `fn` resolvers keep the backend
/// shareable across a real worker boundary.
pub struct WorkerBackend<NK, EK, N, E, S = DefaultBuildHasher>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    S: std::hash::BuildHasher + Default + Clone,
{
    scene: GraphScene<NK, EK, N, E, S>,
    runtime: GraphRuntime<S>,
    inbox: WorkerInbox<NK, EK, N, E>,
    node_label: NodeLabelFn<N>,
    edge_label: EdgeLabelFn<E>,
    node_overlay: NodeOverlayFn,
    edge_overlay: EdgeOverlayFn,
}

impl<NK, EK, N, E, S> WorkerBackend<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    S: std::hash::BuildHasher + Default + Clone + 'static,
{
    /// Create a backend owning `scene`, with no label or overlay resolvers.
    pub fn new(scene: GraphScene<NK, EK, N, E, S>) -> Self {
        Self {
            scene,
            runtime: GraphRuntime::default(),
            inbox: WorkerInbox::new(),
            node_label: |_, _| None,
            edge_label: |_, _| None,
            node_overlay: |_| OverlayCategory::None,
            edge_overlay: |_| OverlayCategory::None,
        }
    }

    /// Resolve node labels with `resolver`.
    pub fn with_node_label(mut self, resolver: NodeLabelFn<N>) -> Self {
        self.node_label = resolver;
        self
    }

    /// Resolve edge labels with `resolver`.
    pub fn with_edge_label(mut self, resolver: EdgeLabelFn<E>) -> Self {
        self.edge_label = resolver;
        self
    }

    /// Resolve node query-overlay categories with `resolver`.
    pub fn with_node_overlay(mut self, resolver: NodeOverlayFn) -> Self {
        self.node_overlay = resolver;
        self
    }

    /// Resolve edge query-overlay categories with `resolver`.
    pub fn with_edge_overlay(mut self, resolver: EdgeOverlayFn) -> Self {
        self.edge_overlay = resolver;
        self
    }

    /// Enqueue one main-thread request.
    pub fn receive(&mut self, request: ToWorker<NK, EK, N, E>) {
        self.inbox.post(request);
    }

    /// Whether no request is pending.
    pub fn is_idle(&self) -> bool {
        self.inbox.is_empty()
    }

    /// The owned replica scene.
    pub fn scene(&self) -> &GraphScene<NK, EK, N, E, S> {
        &self.scene
    }
}

impl<NK, EK, N, E, S> WorkerBackend<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + Sync + 'static,
    EK: Eq + std::hash::Hash + Sync + 'static,
    N: Sync + 'static,
    E: Sync + 'static,
    S: std::hash::BuildHasher + Default + Clone + Sync + 'static,
{
    /// Run one worker cycle over the pending requests.
    ///
    /// Applied in order: every queued mutation (FIFO), then — only when a
    /// frame state was posted — one layout step followed by one indexed frame
    /// build from that state, encoded onto the wire. With no pending frame
    /// state the cycle applies mutations and builds nothing: a frame is only
    /// ever produced for a requested snapshot, keeping the protocol
    /// one-request-in-flight (ADR 0076 §6).
    pub fn step(&mut self) -> Option<FromWorker> {
        let drained = self.inbox.drain();
        for mutation in drained.mutations {
            match mutation {
                SceneMutation::Merge(batch) => {
                    self.scene.merge(batch);
                }
                SceneMutation::Apply(patch) => {
                    self.scene.apply(patch);
                }
                SceneMutation::SetPosition { node, position } => {
                    self.scene.set_position(node, position);
                    // An explicit move is user intent: pin so the next
                    // ForceAtlas2 step keeps the node where it was placed,
                    // mirroring the main-thread drag path.
                    self.scene.pin(node);
                }
            }
        }

        let state = drained.frame_state?;
        self.scene.step_layout(LayoutBudget::default());
        let synced = self.scene.sync_runtime(&mut self.runtime);
        let frame = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &self.node_label,
            edge_label: &self.edge_label,
            viewport: &state.viewport,
            style: &state.style,
            selection: &state.selection,
            hover: &state.hover,
            node_overlay: Some(&self.node_overlay),
            edge_overlay: Some(&self.edge_overlay),
        });
        Some(FromWorker::Frame(PaintFrameWire::encode(&frame)))
    }
}

/// Request-kind tags for [`ToWorker::encode_wire_bytes`] /
/// [`ToWorker::decode_wire_bytes`].
const TAG_SET_POSITION: u8 = 1;
const TAG_FRAME_STATE: u8 = 2;

impl<NK, EK, N, E> ToWorker<NK, EK, N, E> {
    /// Encode the library-owned request content into little-endian wire
    /// bytes for a transferable buffer.
    ///
    /// Covered kinds: [`ToWorker::SetPosition`](Self::Mutation) moves and
    /// [`ToWorker::FrameState`] snapshots. `Merge`/`Apply` carry
    /// application-typed payloads (`NK`/`N`/`EK`/`E`) whose byte form belongs
    /// to an application-supplied payload codec and returns
    /// [`WireFormatError::PayloadCodecRequired`] rather than guessing one.
    pub fn encode_wire_bytes(&self, out: &mut Vec<u8>) -> Result<(), WireFormatError> {
        match self {
            ToWorker::Mutation(SceneMutation::SetPosition { node, position }) => {
                out.push(TAG_SET_POSITION);
                out.extend_from_slice(&node.data().as_ffi().to_le_bytes());
                out.extend_from_slice(&position.x.to_le_bytes());
                out.extend_from_slice(&position.y.to_le_bytes());
                Ok(())
            }
            ToWorker::FrameState(state) => {
                out.push(TAG_FRAME_STATE);
                state.viewport.encode_request_bytes(out);
                encode_selection(&state.selection, out);
                encode_hover(&state.hover, out);
                state.style.encode_request_bytes(out);
                Ok(())
            }
            ToWorker::Mutation(_) => Err(WireFormatError::PayloadCodecRequired),
        }
    }

    /// Parse bytes produced by [`Self::encode_wire_bytes`]. Fail-closed:
    /// truncation, unknown tags, and trailing bytes are errors, never
    /// partially decoded requests.
    pub fn decode_wire_bytes(mut bytes: &[u8]) -> Result<Self, WireFormatError> {
        let tag = take(&mut bytes, 1)?[0];
        let request = match tag {
            TAG_SET_POSITION => {
                let bits =
                    u64::from_le_bytes(take(&mut bytes, 8)?.try_into().expect("eight bytes"));
                let x = f32::from_le_bytes(take(&mut bytes, 4)?.try_into().expect("four bytes"));
                let y = f32::from_le_bytes(take(&mut bytes, 4)?.try_into().expect("four bytes"));
                ToWorker::Mutation(SceneMutation::SetPosition {
                    node: NodeId::from(slotmap::KeyData::from_ffi(bits)),
                    position: Vec2::new(x, y),
                })
            }
            TAG_FRAME_STATE => {
                let viewport = Viewport::decode_request_bytes(&mut bytes)?;
                let selection = decode_selection(&mut bytes)?;
                let hover = decode_hover(&mut bytes)?;
                let style = GraphStyle::decode_request_bytes(&mut bytes)?;
                ToWorker::FrameState(Box::new(FrameState {
                    viewport,
                    style,
                    selection,
                    hover,
                }))
            }
            other => {
                return Err(WireFormatError::BadDiscriminant {
                    field: "request tag",
                    value: other,
                });
            }
        };
        // Every request kind must consume its message exactly.
        if !bytes.is_empty() {
            return Err(WireFormatError::TrailingBytes { extra: bytes.len() });
        }
        Ok(request)
    }
}

/// Borrow-exact prefix split: errors with [`WireFormatError::Truncated`]
/// instead of panicking when `len` bytes are unavailable.
fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], WireFormatError> {
    if bytes.len() < len {
        return Err(WireFormatError::Truncated {
            needed: len,
            remaining: bytes.len(),
        });
    }
    let (head, tail) = bytes.split_at(len);
    *bytes = tail;
    Ok(head)
}

fn push_bits(out: &mut Vec<u8>, id: slotmap::KeyData) {
    out.extend_from_slice(&id.as_ffi().to_le_bytes());
}

fn encode_selection(selection: &Selection, out: &mut Vec<u8>) {
    push_count(out, selection.nodes.len());
    for node in &selection.nodes {
        push_bits(out, node.data());
    }
    push_count(out, selection.edges.len());
    for edge in &selection.edges {
        push_bits(out, edge.data());
    }
}

fn decode_selection(bytes: &mut &[u8]) -> Result<Selection, WireFormatError> {
    let node_count = read_count(bytes)?;
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let bits = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
        nodes.push(NodeId::from(slotmap::KeyData::from_ffi(bits)));
    }
    let edge_count = read_count(bytes)?;
    let mut edges = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        let bits = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
        edges.push(EdgeId::from(slotmap::KeyData::from_ffi(bits)));
    }
    Ok(Selection { nodes, edges })
}

fn encode_hover(hover: &Hover, out: &mut Vec<u8>) {
    encode_optional_id(hover.node, out);
    encode_optional_id(hover.edge, out);
}

fn encode_optional_id(id: Option<impl Key>, out: &mut Vec<u8>) {
    match id {
        Some(id) => {
            out.push(1);
            push_bits(out, id.data());
        }
        None => out.push(0),
    }
}

fn decode_hover(bytes: &mut &[u8]) -> Result<Hover, WireFormatError> {
    Ok(Hover {
        node: decode_optional_id(bytes)?,
        edge: decode_optional_id(bytes)?,
    })
}

fn decode_optional_id<K: Key>(bytes: &mut &[u8]) -> Result<Option<K>, WireFormatError> {
    let present = take(bytes, 1)?;
    if present[0] == 0 {
        return Ok(None);
    }
    let bits = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
    Ok(Some(K::from(slotmap::KeyData::from_ffi(bits))))
}

fn push_count(out: &mut Vec<u8>, count: usize) {
    let count = u64::try_from(count).expect("request element count exceeds u64 range");
    out.extend_from_slice(&count.to_le_bytes());
}

fn read_count(bytes: &mut &[u8]) -> Result<usize, WireFormatError> {
    let raw = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
    usize::try_from(raw).map_err(|_| WireFormatError::ExcessiveLength(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_source::WireFormatError;
    use crate::layout::FixedLayout;
    use crate::style::ArrowShape;
    use gpui::hsla;
    use slotmap::KeyData;

    type TestScene = GraphScene<&'static str, &'static str, (), ()>;
    type TestBackend = WorkerBackend<&'static str, &'static str, (), ()>;
    type TestRequest = ToWorker<&'static str, &'static str, (), ()>;

    /// A two-node scene under FixedLayout so positions are exactly whatever
    /// the test sets — layout stepping cannot move them between builds.
    fn test_scene() -> TestScene {
        let mut scene = TestScene::new();
        scene.merge(GraphBatch::new().node("a", ()).node("b", ()));
        let a = scene.node_id(&"a").unwrap();
        let b = scene.node_id(&"b").unwrap();
        scene.set_position(a, Vec2::new(-10.0, 0.0));
        scene.set_position(b, Vec2::new(10.0, 0.0));
        scene.with_layout(Box::new(FixedLayout))
    }

    fn viewport(size: Vec2) -> Viewport {
        let mut viewport = Viewport::new();
        viewport.set_size(size);
        viewport
    }

    fn frame_state(viewport: Viewport) -> FrameState {
        FrameState {
            viewport,
            style: GraphStyle::default(),
            selection: Selection::new(),
            hover: Hover::default(),
        }
    }

    /// A latest-wins request carrying a default snapshot whose viewport is
    /// sized `size` and centered on the origin.
    fn snapshot(size: Vec2) -> TestRequest {
        ToWorker::FrameState(Box::new(frame_state(viewport(size))))
    }

    fn set_position(node: NodeId, x: f32, y: f32) -> TestRequest {
        ToWorker::Mutation(SceneMutation::SetPosition {
            node,
            position: Vec2::new(x, y),
        })
    }

    #[test]
    fn mutations_queue_fifo_across_interleaved_snapshots() {
        let mut inbox = WorkerInbox::<&'static str, &'static str, (), ()>::new();
        let a = NodeId::from(KeyData::from_ffi(1));
        let b = NodeId::from(KeyData::from_ffi(2));

        inbox.post(set_position(a, 1.0, 1.0));
        inbox.post(snapshot(Vec2::splat(100.0)));
        inbox.post(set_position(b, 2.0, 2.0));
        inbox.post(snapshot(Vec2::splat(200.0)));
        let batch = GraphBatch::<&'static str, &'static str, (), ()>::new().node("c", ());
        inbox.post(ToWorker::Mutation(SceneMutation::Merge(batch)));

        let drained = inbox.drain();

        // Mutations survive in posting order; snapshots collapse latest-wins.
        assert_eq!(drained.mutations.len(), 3);
        match (
            &drained.mutations[0],
            &drained.mutations[1],
            &drained.mutations[2],
        ) {
            (
                SceneMutation::SetPosition { node, position },
                SceneMutation::SetPosition { .. },
                SceneMutation::Merge(_),
            ) => {
                assert_eq!((*node, *position), (a, Vec2::new(1.0, 1.0)));
            }
            _ => panic!("mutation order must be FIFO"),
        }
        assert_eq!(
            drained
                .frame_state
                .expect("snapshot survives")
                .viewport
                .size(),
            Vec2::splat(200.0),
            "the newest snapshot must win"
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn intermediate_snapshots_are_dropped_not_queued() {
        let mut inbox = WorkerInbox::<&'static str, &'static str, (), ()>::new();
        for size in [100.0, 200.0, 300.0] {
            inbox.post(snapshot(Vec2::splat(size)));
        }

        // Exactly one snapshot survives three posts: replacement, not queueing.
        let drained = inbox.drain();
        assert_eq!(
            drained.frame_state.expect("latest wins").viewport.size(),
            Vec2::splat(300.0)
        );

        // A drained inbox stays drained.
        let next = inbox.drain();
        assert!(next.mutations.is_empty());
        assert!(next.frame_state.is_none());
        assert!(inbox.is_empty());
    }

    #[test]
    fn backend_applies_mutations_in_order_then_builds_one_frame() {
        let mut backend = TestBackend::new(test_scene());
        let a = backend.scene().node_id(&"a").unwrap();

        // Drag "a" to the origin while posting a stale-then-fresh snapshot.
        backend.receive(set_position(a, 0.0, 0.0));
        backend.receive(snapshot(Vec2::splat(800.0)));
        let latest = viewport(Vec2::new(600.0, 400.0));
        backend.receive(snapshot(Vec2::new(600.0, 400.0)));

        let response = backend
            .step()
            .expect("a requested snapshot produces a frame");
        // Single-variant destructure: a new variant must update every
        // call site deliberately.
        let FromWorker::Frame(wire) = response;
        let frame = wire.decode();
        assert_eq!(frame.nodes.len(), 2);
        // Frame geometry is canvas-local pixels, so world (0,0) lands at the
        // center of the surviving (newest) snapshot's viewport.
        assert!(
            frame
                .nodes
                .iter()
                .any(|n| n.position == latest.world_to_screen(Vec2::ZERO)),
            "the built frame must reflect the applied drag"
        );

        // One cycle consumes everything: nothing is pending, nothing builds.
        assert!(backend.is_idle());
        assert!(backend.step().is_none());
    }

    #[test]
    fn an_explicit_move_pins_the_replica_node() {
        let mut backend = TestBackend::new(test_scene());
        let a = backend.scene().node_id(&"a").unwrap();
        assert!(!backend.scene().is_pinned(a));

        // A drag crossing the wire is user intent: the replica must hold the
        // node where it was placed instead of letting ForceAtlas2 pull it
        // back toward its force equilibrium on the next step.
        backend.receive(set_position(a, 500.0, 400.0));
        let _ = backend.step(); // applies the queued move (and pins it)
        assert!(backend.scene().is_pinned(a));
    }

    #[test]
    fn backend_without_a_requested_snapshot_builds_nothing() {
        let mut backend = TestBackend::new(test_scene());
        let b = backend.scene().node_id(&"b").unwrap();

        backend.receive(set_position(b, 42.0, -42.0));
        assert!(backend.step().is_none());
        assert_eq!(
            backend.scene().node_position(b),
            Some(Vec2::new(42.0, -42.0))
        );
    }

    #[test]
    fn backend_merge_grows_the_replica_and_the_built_frame() {
        let mut backend = TestBackend::new(test_scene());
        backend.receive(ToWorker::Mutation(SceneMutation::Merge(
            GraphBatch::new().node("c", ()),
        )));
        backend.receive(snapshot(Vec2::splat(800.0)));

        let FromWorker::Frame(wire) = backend.step().expect("builds");
        assert_eq!(wire.decode().nodes.len(), 3);
    }

    #[test]
    fn worker_frame_equals_the_in_process_build_for_identical_inputs() {
        fn label(_id: NodeId, _data: &()) -> Option<String> {
            Some("n".to_string())
        }
        fn overlay(_id: NodeId) -> OverlayCategory {
            OverlayCategory::Emphasized
        }

        // Two independently constructed but byte-identical backends.
        let mut worker_side = TestBackend::new(test_scene())
            .with_node_label(label)
            .with_node_overlay(overlay);
        let reference_scene = test_scene();

        worker_side.receive(snapshot(Vec2::new(800.0, 600.0)));
        let FromWorker::Frame(wire) = worker_side.step().expect("builds");

        let mut runtime = GraphRuntime::default();
        let synced = reference_scene.sync_runtime(&mut runtime);
        let expected = build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &label,
            edge_label: &|_, _| None,
            viewport: &viewport(Vec2::new(800.0, 600.0)),
            style: &GraphStyle::default(),
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: Some(&overlay),
            edge_overlay: None,
        });

        assert_eq!(wire.decode(), expected);
    }

    #[test]
    fn set_position_requests_round_trip_through_wire_bytes() {
        let node = NodeId::from(KeyData::from_ffi(u64::MAX));
        let request = set_position(node, -12.5, f32::MIN_POSITIVE);

        let mut bytes = Vec::new();
        request.encode_wire_bytes(&mut bytes).expect("encodable");
        assert_eq!(bytes.first(), Some(&TAG_SET_POSITION));
        match TestRequest::decode_wire_bytes(&bytes).expect("decodes") {
            TestRequest::Mutation(SceneMutation::SetPosition { node, position }) => {
                assert_eq!(
                    (node.data().as_ffi(), position),
                    (u64::MAX, Vec2::new(-12.5, f32::MIN_POSITIVE))
                );
            }
            _ => panic!("round trip must keep the request kind"),
        }
    }

    #[test]
    fn frame_state_requests_round_trip_every_wire_field() {
        let style = GraphStyle {
            node_radius: 5.5,
            node_min_screen_radius: 1.25,
            node_max_screen_radius: 20.0,
            node_fill: hsla(0.1, 0.2, 0.3, 0.4),
            edge_color_selected: hsla(0.9, 0.8, 0.7, 0.6),
            edge_arrow_enabled: true,
            edge_arrow_shape: ArrowShape::Circle,
            edge_straight_threshold_while_interacting: 12.0,
            ..GraphStyle::default()
        };

        let state = FrameState {
            viewport: {
                let mut vp = Viewport::new();
                vp.set_size(Vec2::new(1024.0, 768.0));
                vp.zoom_at(Vec2::new(100.0, 50.0), 1.5);
                vp.pan(Vec2::new(30.0, -40.0));
                vp
            },
            style,
            selection: Selection {
                nodes: vec![
                    NodeId::from(KeyData::from_ffi(7)),
                    NodeId::from(KeyData::from_ffi(9)),
                ],
                edges: vec![EdgeId::from(KeyData::from_ffi(u64::MAX))],
            },
            hover: Hover {
                node: Some(NodeId::from(KeyData::from_ffi(3))),
                edge: Some(EdgeId::from(KeyData::from_ffi(4))),
            },
        };

        let mut bytes = Vec::new();
        ToWorker::<&'static str, &'static str, (), ()>::FrameState(Box::new(state.clone()))
            .encode_wire_bytes(&mut bytes)
            .expect("encodable");
        let ToWorker::FrameState(decoded) =
            TestRequest::decode_wire_bytes(&bytes).expect("decodes")
        else {
            panic!("round trip must keep the request kind");
        };

        assert_eq!(decoded.viewport.zoom(), state.viewport.zoom());
        assert_eq!(decoded.viewport.center(), state.viewport.center());
        assert_eq!(decoded.viewport.size(), state.viewport.size());
        assert_eq!(decoded.selection, state.selection);
        assert_eq!(decoded.hover, state.hover);
        // Every style field except the deliberately-absent `label_style`.
        assert_eq!(decoded.style.node_radius, state.style.node_radius);
        assert_eq!(
            decoded.style.node_min_screen_radius,
            state.style.node_min_screen_radius
        );
        assert_eq!(
            decoded.style.node_max_screen_radius,
            state.style.node_max_screen_radius
        );
        assert_eq!(decoded.style.node_fill, state.style.node_fill);
        assert_eq!(
            decoded.style.edge_color_selected,
            state.style.edge_color_selected
        );
        assert_eq!(
            decoded.style.edge_arrow_enabled,
            state.style.edge_arrow_enabled
        );
        assert_eq!(decoded.style.edge_arrow_shape, state.style.edge_arrow_shape);
        assert_eq!(
            decoded.style.edge_straight_threshold_while_interacting,
            state.style.edge_straight_threshold_while_interacting
        );
        assert_eq!(
            decoded.style.label_style,
            gpui::TextStyle::default(),
            "label_style is a main-thread-only concern and rides as the default"
        );
    }

    #[test]
    fn batch_payloads_require_an_application_codec_and_bad_bytes_fail_closed() {
        // Application-typed payloads have no library byte form.
        let batch_request = TestRequest::Mutation(SceneMutation::Merge(GraphBatch::new()));
        let mut sink = Vec::new();
        assert_eq!(
            batch_request.encode_wire_bytes(&mut sink),
            Err(WireFormatError::PayloadCodecRequired)
        );
        assert!(sink.is_empty(), "a rejected encoding must not emit bytes");

        // Unknown tags, truncation, and trailing bytes are all rejected.
        match TestRequest::decode_wire_bytes(&[7]) {
            Err(WireFormatError::BadDiscriminant {
                field: "request tag",
                value: 7,
            }) => {}
            other => panic!("unknown tag must be rejected, got {other:?}"),
        }
        assert!(matches!(
            TestRequest::decode_wire_bytes(&[TAG_FRAME_STATE]),
            Err(WireFormatError::Truncated { .. })
        ));

        let mut bytes = Vec::new();
        set_position(NodeId::from(KeyData::from_ffi(1)), 0.0, 0.0)
            .encode_wire_bytes(&mut bytes)
            .unwrap();
        bytes.push(0);
        assert!(matches!(
            TestRequest::decode_wire_bytes(&bytes),
            Err(WireFormatError::TrailingBytes { extra: 1 })
        ));
    }
}
