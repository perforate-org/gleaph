//! wasm32 entrypoint: the `DedicatedWorkerGlobalScope` backend driver.
//! Imported by the application-owned `worker.js` script (`../assets/`); the
//! gpui-graph library itself ships no worker bundle
//! (`gpui_graph::worker::web_transport` contract).

use wasm_bindgen::prelude::*;
use web_sys::DedicatedWorkerGlobalScope;

use gpui_graph::layout::ForceAtlas2;
use gpui_graph::scene::GraphScene;
use gpui_graph::worker::{FromWorker, SceneMutation, ToWorker, WorkerBackend, web_transport};

use crate::batch_codec::{ENVELOPE_LIB_REQUEST, ENVELOPE_MERGE_BATCH, decode_merge_batch};

/// Boot the example's worker-side backend inside
/// `DedicatedWorkerGlobalScope`.
///
/// The replica scene starts empty under ForceAtlas2; its content arrives as an
/// application-encoded merge batch, and its per-frame camera/interaction state
/// arrives as library-encoded snapshots. Every message is answered by one
/// [`WorkerBackend::step`] cycle, keeping the protocol one-request-in-flight
/// with latest-wins backpressure owned by the backend's inbox.
#[wasm_bindgen]
pub fn start_worker() {
    install_panic_hook();
    log("[example-worker] booted in DedicatedWorkerGlobalScope");

    // time_scale 50 mirrors examples/force_atlas2.rs: a gentle multi-second drift.
    let scene = GraphScene::<String, String, String, String>::new()
        .with_layout(Box::new(ForceAtlas2::default().with_time_scale(50.0)));
    let mut backend =
        WorkerBackend::new(scene).with_node_label(|_id, label: &String| Some(label.clone()));

    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let mut frames: usize = 0;
    let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event| {
        handle_message(&mut backend, event, &mut frames);
    });
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

fn handle_message(
    backend: &mut WorkerBackend<String, String, String, String>,
    event: web_sys::MessageEvent,
    frames: &mut usize,
) {
    let Some(bytes) = web_transport::message_bytes(&event) else {
        log("[example-worker] ignoring non-byte message");
        return;
    };
    let Some((envelope, rest)) = bytes.split_first() else {
        log("[example-worker] ignoring empty message");
        return;
    };
    match *envelope {
        ENVELOPE_LIB_REQUEST => match ToWorker::decode_wire_bytes(rest) {
            Ok(request) => {
                if let ToWorker::FrameState(state) = &request {
                    log(&format!(
                        "[example-worker] snapshot: canvas {}x{}, zoom {:.2}",
                        state.viewport.size().x,
                        state.viewport.size().y,
                        state.viewport.zoom(),
                    ));
                }
                backend.receive(request);
            }
            Err(error) => log(&format!("[example-worker] bad library request: {error}")),
        },
        ENVELOPE_MERGE_BATCH => match decode_merge_batch(rest) {
            Ok(batch) => {
                log(&format!(
                    "[example-worker] merged scene batch ({} nodes, {} edges)",
                    batch.nodes.len(),
                    batch.edges.len(),
                ));
                backend.receive(ToWorker::Mutation(SceneMutation::Merge(batch)));
            }
            Err(error) => log(&format!(
                "[example-worker] bad batch codec message: {error}"
            )),
        },
        other => log(&format!("[example-worker] unknown envelope tag {other}")),
    }

    // One cycle per message: mutations apply FIFO, then — only when a snapshot
    // was requested — one layout step and one frame ride back as transferable
    // wire bytes.
    if let Some(FromWorker::Frame(wire)) = backend.step() {
        let wire_bytes = wire.to_wire_bytes();
        *frames += 1;
        if *frames == 1 || (*frames).is_multiple_of(120) {
            log(&format!(
                "[example-worker] built frame #{}: {} wire bytes",
                *frames,
                wire_bytes.len(),
            ));
        }
        if let Err(error) = web_transport::post_response_bytes(wire_bytes) {
            log(&format!("[example-worker] postMessage failed: {error:?}"));
        }
    }
}

fn log(message: &str) {
    web_sys::console::log_1(&JsValue::from_str(message));
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&JsValue::from_str(&format!("{info}")));
    }));
}
