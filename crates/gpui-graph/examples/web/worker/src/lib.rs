//! Application-owned worker bootstrap for the gpui-graph web example.
//!
//! gpui-graph deliberately ships no worker bundle: an application spawns its
//! own Worker script, which imports a wasm module and drives a concrete
//! [`gpui_graph::WorkerBackend`] inside `DedicatedWorkerGlobalScope`
//! (`gpui_graph::worker::web_transport`). This crate is that counterpart for
//! the web example, compiled to wasm32 by `build.sh` and imported by the
//! hand-written `assets/worker.js`.
//!
//! Layout:
//!
//! - [`batch_codec`] — the application-side payload codec for
//!   `SceneMutation::Merge`. Library-encoded requests
//!   ([`gpui_graph::ToWorker::encode_wire_bytes`]) cover node moves and
//!   interaction snapshots only; batch payloads are explicitly left to
//!   applications ([`gpui_graph::frame_source::WireFormatError::PayloadCodecRequired`]
//!   rather than a guessed format). Messages ride under one leading envelope
//!   tag so library bytes cross verbatim.
//! - [`demo`] — the deterministic ~100-vertex demo graph shared by both frame
//!   sources, plus its seed placement.
//!
//! Everything platform-independent (codec, fixture, and the full
//! request→backend→wire round trip) compiles and is unit-tested on native
//! targets; only [`imp`] is wasm-gated.

pub mod batch_codec;
pub mod demo;

#[cfg(target_arch = "wasm32")]
mod imp;

pub use batch_codec::{ENVELOPE_LIB_REQUEST, ENVELOPE_MERGE_BATCH};
pub use demo::{DEMO_NODE_COUNT, demo_batch, initial_position};

#[cfg(test)]
mod tests {
    use glam::Vec2;
    use gpui_graph::layout::ForceAtlas2;
    use gpui_graph::scene::GraphScene;
    use gpui_graph::worker::{FrameState, FromWorker, SceneMutation, ToWorker, WorkerBackend};
    use gpui_graph::{GraphStyle, Hover, PaintFrameWire, Selection, Viewport};

    use crate::batch_codec::{
        ENVELOPE_LIB_REQUEST, ENVELOPE_MERGE_BATCH, decode_merge_batch, encode_merge_batch,
    };
    use crate::demo::{DEMO_NODE_COUNT, demo_batch};

    /// The whole browser conversation, replayed natively: the main thread
    /// encodes a scene merge under the application envelope plus one
    /// library-encoded interaction snapshot; the worker replica decodes both
    /// into a `WorkerBackend`, runs one cycle, and answers with a
    /// `PaintFrameWire` whose transfer bytes re-parse fail-closed. This is
    /// exactly what `assets/worker.js` + `imp.rs` drive in the browser.
    #[test]
    fn worker_pipeline_round_trips_end_to_end_natively() {
        // Worker side: empty replica scene under the ForceAtlas2 engine the
        // example animates with — same construction as `imp::start_worker`.
        let scene = GraphScene::<String, String, String, String>::new()
            .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
        let mut backend =
            WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));

        // Main thread: inject the demo scene as an application-encoded merge.
        let mut merge_message = vec![ENVELOPE_MERGE_BATCH];
        encode_merge_batch(&demo_batch(), &mut merge_message);
        let batch = decode_merge_batch(&merge_message[1..]).expect("merge decodes");
        backend.receive(ToWorker::Mutation(SceneMutation::Merge(batch)));

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
        let mut request_message = vec![ENVELOPE_LIB_REQUEST];
        request
            .encode_wire_bytes(&mut request_message)
            .expect("snapshot is library-encodable");
        backend.receive(
            ToWorker::decode_wire_bytes(&request_message[1..]).expect("request re-decodes"),
        );

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
