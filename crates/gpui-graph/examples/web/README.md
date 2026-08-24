# gpui-graph web example — minimal worker-mode demo

The shortest end-to-end demonstration of gpui-graph's frame-source contract
(DESIGN.md §18.2): the same ~100-vertex ForceAtlas2 graph rendered through the
synchronous **InProcess** source (the library default) or, strictly opt-in,
through a **Worker** that owns a graph backend replica off the main thread.

**Compiling to wasm does not give you a worker.** The wasm build of your app
runs on the main thread exactly like a native build, unless you wire up all
three layers below yourself:

| Layer | Who owns it | What it does |
| --- | --- | --- |
| ① Source selection | your app (a few lines) | `view.connect_worker_channel(Box::new(channel))` then `view.set_frame_source(FrameSource::Worker)` — without both, Worker mode fails loudly instead of silently falling back |
| ② The worker script | your app (`assets/worker.js`) | spawned by the channel as a **module** worker (`PostMessageChannel::spawn_module`), imports *your* worker wasm module, registers the Rust message handler, posts `"ready"`; the channel queues requests until then and replays them in posting order. Classic-worker hosts use `spawn` + `--target no-modules` glue + `importScripts` instead |
| ③ The channel | the library's `web_transport::PostMessageChannel`, configured by your app | implements `WorkerChannel`: spawns the worker, owns the readiness handshake and replay queue, encodes library requests, routes application payload bytes through your [`PayloadCodec`](common/src/batch_codec.rs), decodes `PaintFrameWire` replies, and hands frames to your sink |

What the library provides: the protocol (`ToWorker` / `FromWorker`,
latest-wins backpressure via the backend inbox), the worker-side engine
(`WorkerBackend`) and its message loop (`web_transport::serve`), the frame
transfer form (`PaintFrameWire`), the postMessage primitives, and the generic
channel above. What it deliberately does not provide: your worker bundle,
your scene-merge byte form (batches are application-typed; without a
registered codec, merge requests fail closed with `PayloadCodecRequired` —
see `common/src/batch_codec.rs` for this example's choice), and any bundler
configuration.

## Layout

| Path | Role |
| --- | --- |
| `common/` | Application-owned data both modules share: the deterministic demo fixture and the merge-batch `PayloadCodec`. This is where the example answers the "batch byte forms belong to applications" contract. Plain Rust, unit-tested natively. |
| `app/` | Main-thread GPUI binary: boots `gpui_web` via `run_embedded`, renders ~100 vertices, selects InProcess ⇄ Worker by URL parameter, paints delivered worker frames behind a status overlay. Wasm32-only by construction (no native target gates in its source). |
| `worker/` | Worker-side wasm module: configures a `WorkerBackend` (ForceAtlas2, node labels) and hands it to `web_transport::serve`. One gated entry function; the round trip is unit-tested natively. |
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

Expected browser console in worker mode:

```
[example-worker] booted in DedicatedWorkerGlobalScope
[example-app] queuing scene injection: 100 nodes / ~250 edges
[example-app] worker ready — pending requests flushed
[example-app] frame #1 delivered: 100 nodes / … edges / … labels
```

No COOP/COEP/CORP headers are required or used: everything here is the
no-threads half of the web story (transferable buffers only).

## Checks without a browser

```sh
cargo test                                        # default members: fixture,
                                                  # batch codec, pipe round trip
cargo clippy                                      # same scope
cargo check --target wasm32-unknown-unknown \
    -p gpui-graph-web-example-app                 # API-drift net, app side
cargo check --target wasm32-unknown-unknown \
    -p gpui-graph-web-example-worker              # …and worker side
```

The example consumes only public gpui-graph APIs from outside the library, so
any upstream breakage surfaces at `cargo check` time, before any browser is
involved. Bare `cargo test` / `cargo clippy` intentionally skip `app/`
(default-members): the app binary touches browser-only library surfaces with
no target gates in its source and is wasm32-only by construction.

## Toolchain notes

Plain `build.sh` is the canonical path here: two explicit
`cargo build --target wasm32-unknown-unknown` invocations plus two
`wasm-bindgen --target web` steps. Bundler-style dev servers automate only
part of this shape — the example is inherently **two** modules (main-thread
app + worker) with different entry points — so a plain script stays the
clearest source of truth. Product hosts routinely go further and let vite /
webpack own hashing and code splitting; both tools wrap the same two
wasm-bindgen outputs, so nothing in this example changes when you swap the
driver script for a bundler.

## Embedding in a framework host (Leptos / Dioxus / …)

The three layers above are identical inside a framework-hosted page. gpui_web
self-draws into its own canvas and needs no DOM participation from a
framework, so the framework mounts one element (or nothing at all), and layers
①–③ remain: connect the channel, select `FrameSource::Worker`, own
`worker.js` and the worker wasm. A framework host changes where `init()` is
called from — not what the wiring looks like.

## Notes

- This directory is a standalone mini-workspace so web-only dependency churn
  never touches an enclosing product workspace's lockfile. `app` is excluded
  from default-members because its sources are unconditionally browser-only;
  build it explicitly, as `build.sh` does.
- Keep `main.rs` out of this directory itself — cargo would auto-discover
  `examples/web/main.rs` as an example target of `gpui-graph`.
- Drag edits are not mirrored to the worker replica in this example
  (`SceneMutation::SetPosition` exists and crosses the library envelope;
  hosts that want live drags send it through `PipeHandle`).
