//! Application-owned worker bootstrap for the gpui-graph web example.
//!
//! gpui-graph deliberately ships no worker bundle: an application spawns its
//! own Worker script (`assets/worker.js`), which imports a wasm module and
//! drives a concrete [`gpui_graph::WorkerBackend`] inside
//! `DedicatedWorkerGlobalScope`. This crate is that wasm module — and after
//! the channel extraction it is nearly empty: the message loop, envelope
//! routing, readiness handshake, and replay queue all live in the library
//! ([`gpui_graph::worker::web_transport`]), so what remains here is exactly
//! the application's own content: the backend configuration (which layout,
//! which label resolver) and the payload codec choice.
//!
//! The whole browser conversation is replayed natively in the test below:
//! main thread encodes a merge batch under the application codec plus one
//! library-encoded snapshot; the pipe core routes both into a `WorkerBackend`;
//! one cycle answers with a `PaintFrameWire` that re-parses fail-closed.

//! The whole browser conversation is replayed natively in the test below:
//! main thread encodes a merge batch under the application codec plus one
//! library-encoded snapshot; the pipe core routes both into a `WorkerBackend`;
//! one cycle answers with a `PaintFrameWire` that re-parses fail-closed.

/// The whole browser conversation is replayed natively in the test below:
/// main thread encodes a merge batch under the application codec plus one
/// library-encoded snapshot; the pipe core routes both into a `WorkerBackend`;
/// one cycle answers with a `PaintFrameWire` that re-parses fail-closed.

// The entry is the crate's only wasm-only item (one gated import + one gated
// attribute); its other imports live inside the body so native builds stay
// clean.
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn start_worker() {
    use gpui_graph::layout::ForceAtlas2;
    use gpui_graph::scene::GraphScene;
    use gpui_graph::worker::{WorkerBackend, web_transport};
    use gpui_graph_web_example_common::DemoBatchCodec;

    let scene = GraphScene::<String, String, String, String>::new()
        .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
    let backend =
        WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));

    // Registers the message handler and returns; `assets/worker.js` posts the
    // readiness signal right after this resolves, and the library's replay
    // queue orders any request the main thread made in between.
    web_transport::serve(backend, Box::new(DemoBatchCodec));
}

#[cfg(test)]
mod tests {
    use glam::Vec2;
    use gpui_graph::layout::ForceAtlas2;
    use gpui_graph::scene::GraphScene;
    use gpui_graph::worker::{
        FrameState, FromWorker, Inbound, SceneMutation, ToWorker, WorkerBackend, decode_inbound,
        encode_request,
    };
    use gpui_graph::{GraphStyle, Hover, PaintFrameWire, Selection, Viewport};

    use gpui_graph_web_example_common::{DEMO_NODE_COUNT, DemoBatchCodec, demo_batch};

    /// The whole browser conversation, replayed natively through the same
    /// entry points `assets/worker.js` + `start_worker` drive in the browser:
    /// a merge batch crosses under the application codec's envelope, a
    /// snapshot under the library envelope, and one backend cycle answers
    /// with a wire that re-parses fail-closed.
    #[test]
    fn worker_pipeline_round_trips_end_to_end_natively() {
        // Worker side: empty replica scene under the ForceAtlas2 engine the
        // example animates with — same construction as `start_worker`.
        let scene = GraphScene::<String, String, String, String>::new()
            .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
        let mut backend =
            WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));
        let codec = DemoBatchCodec;

        // Main thread: inject the demo scene as an application-encoded merge.
        let mut merge_message = Vec::new();
        encode_request(
            &ToWorker::Mutation(SceneMutation::Merge(demo_batch())),
            Some(&codec),
            &mut merge_message,
        )
        .expect("the example codec covers merges");
        match decode_inbound(&merge_message, Some(&codec)).expect("merge decodes") {
            Inbound::App(SceneMutation::Merge(batch)) => {
                backend.receive(ToWorker::Mutation(SceneMutation::Merge(batch)));
            }
            other => {
                panic!("an app-mutation envelope must route to Inbound::App(Merge), got {other:?}")
            }
        }

        // Main thread: post one snapshot through the library byte form.
        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(800.0, 600.0));
        let request =
            ToWorker::<String, String, String, String>::FrameState(Box::new(FrameState {
                viewport,
                style: GraphStyle::default(),
                selection: Selection::new(),
                hover: Hover::default(),
            }));
        let mut request_message = Vec::new();
        encode_request(&request, None, &mut request_message).expect("snapshots are library-owned");
        match decode_inbound(&request_message, Some(&codec)).expect("snapshot decodes") {
            Inbound::Library(decoded) => backend.receive(decoded),
            Inbound::App(_) => panic!("a library envelope must route to Inbound::Library"),
        }

        // One worker cycle: apply mutations FIFO, step layout, build one frame.
        let response = backend.step().expect("a requested snapshot builds a frame");
        let FromWorker::Frame(wire) = response;

        // The delivered wire bytes survive the transfer form exactly.
        let reparsed = PaintFrameWire::from_wire_bytes(&wire.to_wire_bytes())
            .expect("transfer bytes must re-parse");
        let frame = reparsed.decode();
        assert_eq!(
            frame.nodes.len(),
            DEMO_NODE_COUNT,
            "every injected vertex reaches the built frame"
        );
        assert!(
            !frame.labels.is_empty(),
            "node labels ride along via the label resolver"
        );

        // One cycle consumes everything: no pending request builds again.
        assert!(backend.is_idle());
        assert!(backend.step().is_none());
    }
}
