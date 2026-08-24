// Application-owned ADR 0076 worker bootstrap (S4a).
//
// gpui-graph ships no worker bundle: `gpui_graph::worker::web_transport`
// documents that an application spawns its own Worker script, imports its wasm
// module, and drives a concrete `WorkerBackend` inside
// DedicatedWorkerGlobalScope. This file is that script, spawned by the main
// thread as a module worker (`new Worker('worker.js', { type: 'module' })`).
//
// Protocol: every backend message is one transferable ArrayBuffer whose first
// byte is an application envelope tag (see
// `gleaph-explorer-web-worker::batch_codec`). Because wasm initialization is
// asynchronous, this script posts the plain string `"ready"` once the Rust
// message handler is registered; the main thread holds every request until
// then and replays them in posting order.
import init, { start_worker } from './gleaph_explorer_web_worker.js';

init()
    .then(() => {
        console.log('[explorer-worker] wasm initialized, starting backend');
        start_worker();
        self.postMessage('ready');
    })
    .catch((error) => console.error('[explorer-worker] wasm init failed:', error));
