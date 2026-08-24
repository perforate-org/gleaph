//! wasm32 entrypoints: the DedicatedWorkerGlobalScope backend driver and the
//! timing-harness export. Imported by the application-owned `worker.js` /
//! `bench_worker.js` scripts (`web/assets/`); gpui-graph itself ships no worker
//! bundle (`gpui_graph::worker::web_transport`).

use wasm_bindgen::prelude::*;

use gpui_graph::layout::ForceAtlas2;
use gpui_graph::scene::GraphScene;
use gpui_graph::worker::{FromWorker, SceneMutation, ToWorker, WorkerBackend, web_transport};

use crate::batch_codec;
use crate::paint_timing;

/// Boot the explorer's worker-side backend inside
/// `DedicatedWorkerGlobalScope`.
///
/// The replica scene starts empty with the ForceAtlas2 engine the demo animates
/// with; its content arrives as application-encoded merge batches, and its
/// per-frame camera/interaction state arrives as library-encoded snapshots.
/// Every message is answered by one [`WorkerBackend::step`] cycle, keeping the
/// protocol one-request-in-flight with latest-wins backpressure owned by the
/// inbox.
#[wasm_bindgen]
pub fn start_worker() {
    install_panic_hook();
    log("[explorer-worker] booted in DedicatedWorkerGlobalScope");

    // time_scale mirrors examples/force_atlas2.rs: a gentle multi-second drift.
    let scene = GraphScene::<String, String, String, String>::new()
        .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
    let mut backend =
        WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));

    let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let mut snapshots: usize = 0;
    let mut frames: usize = 0;
    let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event| {
        handle_message(&mut backend, event, &mut snapshots, &mut frames);
    });
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

fn handle_message(
    backend: &mut WorkerBackend<String, String, String, String>,
    event: web_sys::MessageEvent,
    snapshots: &mut usize,
    frames: &mut usize,
) {
    let Some(bytes) = web_transport::message_bytes(&event) else {
        log("[explorer-worker] ignoring non-byte message");
        return;
    };
    let Some((envelope, rest)) = bytes.split_first() else {
        log("[explorer-worker] ignoring empty message");
        return;
    };
    match *envelope {
        batch_codec::ENVELOPE_LIB_REQUEST => match ToWorker::decode_wire_bytes(rest) {
            Ok(request) => {
                if let ToWorker::FrameState(state) = &request {
                    *snapshots += 1;
                    if *snapshots == 1 || (*snapshots).is_multiple_of(120) {
                        log(&format!(
                            "[explorer-worker] snapshot #{}: canvas {}x{}, zoom {:.2}",
                            *snapshots,
                            state.viewport.size().x,
                            state.viewport.size().y,
                            state.viewport.zoom(),
                        ));
                    }
                }
                backend.receive(request);
            }
            Err(error) => log(&format!("[explorer-worker] bad library request: {error}")),
        },
        batch_codec::ENVELOPE_MERGE_BATCH => match batch_codec::decode_merge_batch(rest) {
            Ok(batch) => {
                let nodes = batch.nodes.len();
                let edges = batch.edges.len();
                backend.receive(ToWorker::Mutation(SceneMutation::Merge(batch)));
                log(&format!(
                    "[explorer-worker] merged scene batch ({nodes} nodes, {edges} edges)"
                ));
            }
            Err(error) => log(&format!(
                "[explorer-worker] bad batch codec message: {error}"
            )),
        },
        other => log(&format!("[explorer-worker] unknown envelope tag {other}")),
    }

    if let Some(FromWorker::Frame(wire)) = backend.step() {
        let wire_bytes = wire.to_wire_bytes();
        *frames += 1;
        if *frames == 1 || (*frames).is_multiple_of(120) {
            let counts = {
                let frame = wire.decode();
                format!(
                    "{} nodes / {} edges / {} labels",
                    frame.nodes.len(),
                    frame.edges.len(),
                    frame.labels.len()
                )
            };
            log(&format!(
                "[explorer-worker] frame #{} built: {counts}, {} wire bytes",
                *frames,
                wire_bytes.len()
            ));
        }
        if let Err(error) = web_transport::post_response_bytes(wire_bytes) {
            log(&format!("[explorer-worker] postMessage failed: {error:?}"));
        }
    }
}

/// Run the paint-frame timing harness on this target and return the stats as a
/// JSON object string.
#[wasm_bindgen]
pub fn run_paint_benchmark(nodes: usize, iterations: usize) -> Result<JsValue, JsValue> {
    if nodes == 0 || iterations == 0 {
        return Err(JsValue::from_str("nodes and iterations must be non-zero"));
    }
    install_panic_hook();
    let started = web_time::Instant::now();
    let stats = paint_timing::measure_paint_build(nodes, iterations);
    log(&format!(
        "[explorer-bench] {nodes} nodes × {iterations} iters in {:?}",
        started.elapsed()
    ));
    Ok(JsValue::from_str(
        &serde_json::to_string(&stats).expect("stats serialize"),
    ))
}

fn log(message: &str) {
    web_sys::console::log_1(&JsValue::from_str(message));
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&JsValue::from_str(&format!("{info}")));
    }));
}
