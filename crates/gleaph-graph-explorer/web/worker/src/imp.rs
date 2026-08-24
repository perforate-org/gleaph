//! wasm32 entrypoints: the DedicatedWorkerGlobalScope backend driver and the
//! timing-harness export. Imported by the application-owned `worker.js` /
//! `bench_worker.js` scripts (`web/assets/`); gpui-graph itself ships no worker
//! bundle (`gpui_graph::worker::web_transport`).
//!
//! After the channel extraction the backend driver is exactly what a host
//! genuinely owns: the backend configuration (layout engine, label resolver)
//! handed to [`web_transport::serve`] — the message loop, envelope routing,
//! readiness handshake, and frame posting all live in the library.

use wasm_bindgen::prelude::*;

use gpui_graph::layout::ForceAtlas2;
use gpui_graph::scene::GraphScene;
use gpui_graph::worker::{WorkerBackend, web_transport};

use crate::paint_timing;
use gleaph_explorer_web_common::ExplorerBatchCodec;

/// Boot the explorer's worker-side backend inside
/// `DedicatedWorkerGlobalScope`.
///
/// The replica scene starts empty with the ForceAtlas2 engine the demo animates
/// with; its content arrives as application-encoded merge batches, and its
/// per-frame camera/interaction state arrives as library-encoded snapshots.
/// `serve` registers the message loop: every inbound message is decoded into
/// the backend inbox and answered by one backend cycle, keeping the protocol
/// one-request-in-flight with latest-wins backpressure owned by the inbox.
#[wasm_bindgen]
pub fn start_worker() {
    log("[explorer-worker] booted in DedicatedWorkerGlobalScope");

    // time_scale mirrors examples/force_atlas2.rs: a gentle multi-second drift.
    let scene = GraphScene::<String, String, String, String>::new()
        .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
    let backend =
        WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));

    web_transport::serve(backend, Box::new(ExplorerBatchCodec));
}

/// Run the paint-frame timing harness on this target and return the stats as a
/// JSON object string.
///
/// The timing harness is a separate protocol from the graph backend (its
/// `bench_worker.js` script owns the conversation), so it stays outside
/// `serve`.
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
