# gpui-graph web example — minimal worker-mode demo

The shortest end-to-end demonstration of gpui-graph's frame-source contract
(DESIGN.md §18.2, ADR 0076): the same ~100-vertex ForceAtlas2 graph rendered
through the synchronous **InProcess** source (the library default) or, strictly
opt-in, through a **Worker** that owns a graph backend replica off the main
thread.

**Compiling to wasm does not give you a worker.** gpui-graph ships no worker
bundle — by contract (`gpui_graph::worker::web_transport`). The wasm build of
your app runs on the main thread exactly like a native build, and frames are
built in-process, unless you wire up all three layers below yourself:

| Layer | Who owns it | What it does |
| --- | --- | --- |
| ① Source selection | your app (a few lines) | `view.connect_worker_channel(Box::new(channel))` then `view.set_frame_source(FrameSource::Worker)` — without both, Worker mode fails loudly instead of silently falling back |
| ② The worker script | your app (`assets/worker.js`) | spawns as an ES module worker, imports *your* worker wasm module, registers the Rust message handler, posts `"ready"`; the main thread queues requests until then and replays them |
| ③ The channel implementation | your app (`WebWorkerChannel` in `app/src/main.rs`) | implements `WorkerChannel`: encodes requests (`ToWorker::encode_wire_bytes`), ships bytes through `web_transport` transferable `ArrayBuffer`s, parses replies (`PaintFrameWire::from_wire_bytes`) and calls `deliver_worker_frame` |

What the library *does* provide: the protocol (`ToWorker` / `FromWorker`,
latest-wins backpressure via the backend inbox), the worker-side engine
(`WorkerBackend`: apply mutations FIFO → one layout step → one indexed frame),
the frame transfer form (`PaintFrameWire`), and the thin postMessage glue
(`web_transport`). What it deliberately does not provide: your worker bundle,
your scene-merge byte form (batches are application-typed;
`encode_wire_bytes` answers `PayloadCodecRequired` rather than guessing — see
`worker/src/batch_codec.rs` for a minimal application codec), and any bundler
configuration.

## Layout

| Path | Role |
| --- | --- |
| `app/` | Main-thread GPUI binary: boots `gpui_web` via `run_embedded`, renders ~100 vertices, selects InProcess ⇄ Worker by URL parameter, paints delivered worker frames behind a status overlay. |
| `worker/` | Application-owned worker wasm: drives `WorkerBackend` per message (library requests verbatim under envelope tag 1, application merge batches under tag 2). The codec, demo fixture, and full request→backend→wire round trip are plain Rust, unit-tested natively. |
| `assets/worker.js` | Layer ②: application-owned module-worker bootstrap with the `"ready"` handshake. |
| `assets/index.html` | App page. |
| `build.sh` | Builds both wasm modules into `dist/`. |

## Build & run

```sh
cd crates/gpui-graph/examples/web
./build.sh
python3 -m http.server 8080 --directory dist   # any static server works
```

- <http://127.0.0.1:8080/> — InProcess (library default): layout steps locally,
  frames build synchronously.
- <http://127.0.0.1:8080/?mode=worker> — opt-in Worker source: the main thread
  injects the scene and paints delivered frames; the status overlay counts
  deliveries.

No COOP/COEP/CORP headers are required or used: everything here is the
no-threads half of the web story (transferable buffers only).

Expected browser console in worker mode:

```
[example-worker] booted in DedicatedWorkerGlobalScope
[example-app] queuing scene injection: 100 nodes / ~250 edges
[example-app] worker ready — pending requests flushed
[example-app] frame #1 delivered: 100 nodes / … edges / … labels
```

## Checks without a browser

```sh
cargo test                                   # native: codec + full worker round trip
cargo check --target wasm32-unknown-unknown -p gpui-graph-web-example-app
cargo check --target wasm32-unknown-unknown -p gpui-graph-web-example-worker
```

The wasm32 checks are the mechanical API-drift net: this example consumes only
public gpui-graph APIs from outside the library, so breakage surfaces at
`cargo check` time, before any browser is involved.

## Toolchain notes

Plain `build.sh` is the canonical path: two explicit `cargo build --target
wasm32-unknown-unknown` invocations plus two `wasm-bindgen --target web` steps.
Trunk is an optional DX route for single-module apps, but this example is
inherently **two** modules (main-thread app + worker) with different entry
points, so Trunk's single-index automation covers roughly half the wiring and a
plain script stays the clearest source of truth. Product hosts routinely go
further and let vite/webpack own hashing and code splitting; both tools wrap
the same two wasm-bindgen outputs, so nothing in this example changes when you
swap the driver script for a bundler.

## Embedding in a framework host (Leptos / Dioxus / …)

The three layers above are identical inside a framework-hosted page. gpui_web
self-draws into its own canvas and needs no DOM participation from a framework,
so the framework mounts one element (or nothing at all), and layers ①–③ remain:
connect the channel, select `FrameSource::Worker`, own `worker.js` and the
worker wasm. A Leptos/Dioxus host changes where `init()` is called from — not
what the wiring looks like.

## Notes

- This directory is a standalone mini-workspace (explicit `[workspace]`,
  like `demo/social/wasm/`) so root-workspace members and lockfile stay
  untouched by web-only work; dependency revisions are restated to unify with
  the root lockfile. Keep `main.rs` out of this directory itself — cargo would
  auto-discover `examples/web/main.rs` as an example target of `gpui-graph`.
- Drag edits are not mirrored to the worker replica in this example
  (`SceneMutation::SetPosition` exists for hosts that want it).
- Extracting the explorer's generic postMessage channel (envelope tags +
  readiness handshake + pending replay) into a reusable reference type is
  recorded as future work; this example intentionally hand-rolls the minimum
  instead of growing the library.
