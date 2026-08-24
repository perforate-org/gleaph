// Application-owned worker script for the ADR 0076 S4a timing harness.
//
// Same contract as `worker.js` (application-supplied script importing the wasm
// module), but instead of driving the message-protocol backend it runs the
// paint-frame build repeatedly and posts the stats JSON back. Spawned by
// harness.html with ?nodes=&iterations= query parameters.
import init, { run_paint_benchmark } from './gleaph_explorer_web_worker.js';

const parameters = new URLSearchParams(self.location.search);
const nodes = Number(parameters.get('nodes') ?? 5000);
const iterations = Number(parameters.get('iterations') ?? 30);

init()
    .then(() => {
        console.log(`[explorer-bench] running: nodes=${nodes} iterations=${iterations}`);
        const json = run_paint_benchmark(nodes, iterations);
        self.postMessage({ kind: 'result', nodes, json });
    })
    .catch((error) => {
        console.error('[explorer-bench] failed:', error);
        self.postMessage({ kind: 'error', nodes, message: String(error) });
    });
