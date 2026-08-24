//! ADR 0076 S4a application-owned worker bootstrap for the graph explorer web
//! entry.
//!
//! `gpui-graph` deliberately ships no worker bundle: an application spawns its
//! own Worker script, which imports a wasm module and drives a concrete
//! [`gpui_graph::WorkerBackend`] inside `DedicatedWorkerGlobalScope`
//! (`gpui_graph::worker::web_transport`). This crate is that counterpart for
//! the explorer web entry, compiled to wasm32 by `web/build.sh` and imported by
//! the hand-written `assets/worker.js`.
//!
//! After the channel extraction this crate owns exactly what a host genuinely
//! owns:
//!
//! - the backend configuration handed to [`web_transport::serve`] in `imp`
//!   (ForceAtlas2 engine, node-label resolver) — the message loop, envelope
//!   routing, readiness handshake, and frame posting all live in the library;
//! - [`paint_timing`] — the pure timing-harness core: one indexed paint-frame
//!   build over the deterministic fixture shared through
//!   `gleaph-explorer-web-common`, timed as-is on whatever target runs it. The
//!   wasm export wraps this core, so browser numbers and native baseline
//!   numbers come out of identical code.
//!
//! Application-owned data — the merge-batch payload codec and the demo
//! fixture — lives in `gleaph-explorer-web-common`, shared with the main-thread
//! app so neither wasm module duplicates it.
//!
//! Everything platform-independent is compiled and tested on native targets;
//! only the `imp` module is wasm-gated.

pub mod paint_timing;

#[cfg(target_arch = "wasm32")]
mod imp;

#[cfg(test)]
mod tests {
    use glam::Vec2;
    use gleaph_explorer_web_common::{ExplorerBatchCodec, random_fixture};
    use gpui_graph::layout::ForceAtlas2;
    use gpui_graph::scene::GraphScene;
    use gpui_graph::worker::{
        FrameState, FromWorker, Inbound, SceneMutation, ToWorker, WorkerBackend, decode_inbound,
        encode_request,
    };
    use gpui_graph::{GraphStyle, Hover, PaintFrameWire, Selection, Viewport};

    /// The whole browser conversation, replayed natively through the same
    /// entry points `assets/worker.js` + `start_worker` drive in the browser:
    /// a merge batch crosses under the application codec's envelope, a
    /// snapshot under the library envelope, and one backend cycle answers
    /// with a wire that re-parses fail-closed.
    #[test]
    fn worker_pipeline_round_trips_end_to_end_natively() {
        // Worker side: empty replica scene under the ForceAtlas2 engine the
        // entry animates with — same construction as `start_worker`.
        let scene = GraphScene::<String, String, String, String>::new()
            .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
        let mut backend =
            WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));
        let codec = ExplorerBatchCodec;

        // Main thread: inject the demo scene as an application-encoded merge.
        let node_count = 120;
        let mut merge_message = Vec::new();
        encode_request(
            &ToWorker::Mutation(SceneMutation::Merge(random_fixture(node_count).batch)),
            Some(&codec),
            &mut merge_message,
        )
        .expect("the explorer codec covers merges");
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
            node_count,
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
