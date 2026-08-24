//! Thin postMessage transport for the ADR 0076 worker host (web only).
//!
//! This module moves bytes across the Worker boundary; every decision about
//! what the bytes contain belongs to the protocol layer ([`crate::worker`])
//! and its callers. Frames ride one transferable `ArrayBuffer` each
//! (sub-millisecond at measured sizes); structured cloning of frame data is
//! explicitly out per ADR 0076 §3.
//!
//! The library cannot ship a worker bundle: an application spawns its own
//! Worker script (which imports this wasm module and drives a concrete
//! [`WorkerBackend`](crate::worker::WorkerBackend) instantiation inside
//! `DedicatedWorkerGlobalScope`), then wires it up through these helpers.

use wasm_bindgen::JsCast;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, Worker};

/// Spawn a worker from an application-supplied script URL.
pub fn spawn_worker(script_url: &str) -> Result<Worker, wasm_bindgen::JsValue> {
    Worker::new(script_url)
}

/// Send one request message to `worker`, transferring the backing buffer.
pub fn send_request(worker: &Worker, bytes: Vec<u8>) -> Result<(), wasm_bindgen::JsValue> {
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let transfer = js_sys::Array::new();
    transfer.push(&array.buffer().into());
    worker.post_message_with_transfer(&array.into(), &transfer.into())
}

/// Extract the request bytes from one main-thread message, or `None` for
/// messages that are not byte arrays.
pub fn message_bytes(event: &MessageEvent) -> Option<Vec<u8>> {
    event
        .data()
        .dyn_into::<js_sys::Uint8Array>()
        .ok()
        .map(|array| array.to_vec())
}

/// Send one response message to the main thread from inside the worker,
/// transferring the backing buffer.
pub fn post_response_bytes(bytes: Vec<u8>) -> Result<(), wasm_bindgen::JsValue> {
    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let transfer = js_sys::Array::new();
    transfer.push(&array.buffer().into());
    scope.post_message_with_transfer(&array.into(), &transfer.into())
}
