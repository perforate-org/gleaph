// Application-owned ADR 0076 worker bootstrap (S4a).
//
// gpui-graph ships no worker bundle: `gpui_graph::worker::web_transport`
// documents that an application spawns its own Worker script, imports its wasm
// module, and drives a concrete `WorkerBackend` inside
// DedicatedWorkerGlobalScope. The library's `PostMessageChannel` spawns this
// script with `new Worker(url)` — a CLASSIC dedicated worker — so ES module
// syntax is unavailable here: `build.sh` emits the wasm-bindgen glue with
// `--target no-modules`, loaded via importScripts.
//
// Protocol: every backend message is one transferable ArrayBuffer whose first
// byte is an envelope tag owned by the library (`worker::pipe_core::envelope`;
// application batch bytes ride `APP_MUTATION` through the explorer's codec in
// `gleaph-explorer-web-worker::batch_codec`). Because wasm initialization is
// asynchronous, this script posts the plain string "ready" once the Rust
// message handler is registered; the library channel holds every request until
// then and replays them in posting order.
importScripts('./gleaph_explorer_web_worker.js');

wasm_bindgen({ module_or_path: './gleaph_explorer_web_worker_bg.wasm' })
    .then(() => {
        console.log('[explorer-worker] wasm initialized, starting backend');
        wasm_bindgen.start_worker();
        self.postMessage('ready');
    })
    .catch((error) => console.error('[explorer-worker] wasm init failed:', error));
