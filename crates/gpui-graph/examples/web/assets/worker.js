// Application-owned worker bootstrap (layer ② in README terms).
//
// gpui-graph ships no worker bundle: `gpui_graph::worker::web_transport`
// documents that an application spawns its own Worker script, imports its wasm
// module, and drives a concrete `WorkerBackend` inside
// DedicatedWorkerGlobalScope. This file is that script, spawned by the main
// thread as a module worker (`new Worker('worker.js', { type: 'module' })` —
// see app/src/main.rs, `WebWorkerChannel::spawn`).
//
// Protocol: every backend message is one transferable ArrayBuffer whose first
// byte is an application envelope tag (see
// `gpui-graph-web-example-worker::batch_codec`). Because wasm initialization is
// asynchronous, this script posts the plain string `"ready"` once the Rust
// message handler is registered; the main thread holds every request until
// then and replays them in posting order.
import init, { start_worker } from './gpui_graph_web_example_worker.js';

init()
    .then(() => {
        console.log('[example-worker] wasm initialized, starting backend');
        start_worker();
        self.postMessage('ready');
    })
    .catch((error) => console.error('[example-worker] wasm init failed:', error));
